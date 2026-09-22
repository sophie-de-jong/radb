//! Section 7.3 — Semantics. Table rows #18–#25.
use radb::{parse_query, Engine, Relation, SemanticError, Value};

/// Build the canonical relation from Section 4.1:
///
///   Employees (EID, Name, Age, DID) =
///     E1, John, 32, D1
///     E2, Alice, 28, D2
///     E3, Bob, 29, D1
fn employees() -> Relation {
    let mut rel = Relation::new(["EID", "Name", "Age", "DID"]);
    for row in [
        [
            Value::Str("E1".into()),
            Value::Str("John".into()),
            Value::Int(32),
            Value::Str("D1".into()),
        ],
        [
            Value::Str("E2".into()),
            Value::Str("Alice".into()),
            Value::Int(28),
            Value::Str("D2".into()),
        ],
        [
            Value::Str("E3".into()),
            Value::Str("Bob".into()),
            Value::Int(29),
            Value::Str("D1".into()),
        ],
    ] {
        rel.push(row).expect("static test data is well-formed");
    }
    rel
}

/// A small departments relation to pair with Employees for join tests.
///
///   Departments (DID, DName) =
///     D1, Engineering
///     D2, Sales
fn departments() -> Relation {
    let mut rel = Relation::new(["DID", "DName"]);
    for row in [
        [Value::Str("D1".into()), Value::Str("Engineering".into())],
        [Value::Str("D2".into()), Value::Str("Sales".into())],
    ] {
        rel.push(row).expect("static test data is well-formed");
    }
    rel
}

/// Employees with an extra "MgrID" column for the self-join test (#20).
///
///   Emp (EID, Name, Age, DID, MgrID) =
///     E1, John,  32, D1, E3
///     E2, Alice, 28, D2, E1
///     E3, Bob,   29, D1, — 
fn employees_with_mgr() -> Relation {
    let mut rel = Relation::new(["EID", "Name", "Age", "DID", "MgrID"]);
    for row in [
        [
            Value::Str("E1".into()),
            Value::Str("John".into()),
            Value::Int(32),
            Value::Str("D1".into()),
            Value::Str("E3".into()),
        ],
        [
            Value::Str("E2".into()),
            Value::Str("Alice".into()),
            Value::Int(28),
            Value::Str("D2".into()),
            Value::Str("E1".into()),
        ],
        [
            Value::Str("E3".into()),
            Value::Str("Bob".into()),
            Value::Int(29),
            Value::Str("D1".into()),
            Value::Str("E3".into()),
        ],
    ] {
        rel.push(row).expect("static test data is well-formed");
    }
    rel
}

#[test]
fn test_18_select_attribute_vs_attribute() {
    let mut eng = Engine::new();
    let mut r = Relation::new(["A", "B"]);
    for row in [
        [Value::Int(1), Value::Int(1)],
        [Value::Int(1), Value::Int(2)],
        [Value::Int(2), Value::Int(2)],
    ] {
        r.push(row).expect("static test data is well-formed");
    }
    eng.load("R", r);
    let expr = parse_query("select[A=B](R)").unwrap();
    let result = eng.execute(&expr).expect("query should succeed");
    assert_eq!(result.schema(), ["A", "B"]);
    // Only (1,1) and (2,2) satisfy A=B
    assert_eq!(result.len(), 2);
    assert!(result.contains([Value::Int(1), Value::Int(1)]));
    assert!(result.contains([Value::Int(2), Value::Int(2)]));
}

#[test]
fn test_19_join_qualified_names() {
    let mut eng = Engine::new();
    eng.load("Emp", employees());
    eng.load("Dept", departments());
    let expr = parse_query("Emp join[Emp.DID=Dept.DID] Dept").unwrap();
    let result = eng.execute(&expr).expect("query should succeed");

    assert_eq!(
        result.schema(),
        [
            "Emp.EID",
            "Emp.Name",
            "Emp.Age",
            "Emp.DID",
            "Dept.DID",
            "Dept.DName"
        ]
    );

    // Every output row must have the same DID in both DID columns.
    for row in result.iter() {
        let emp_did = &row[3];
        let dept_did = &row[4];
        assert_eq!(emp_did, dept_did, "DID columns must match in join output");
    }

    // Employees E1 (D1) and E3 (D1) join with D1; E2 (D2) joins with D2.
    assert_eq!(result.len(), 3);
}

#[test]
fn test_20_self_join_with_rename() {
    let mut eng = Engine::new();
    eng.load("Emp", employees_with_mgr());

    // rename[E2](Emp) join[Emp.MgrID=E2.EID] Emp
    // This pairs each employee with their manager.
    let expr = parse_query("rename[E2](Emp) join[Emp.MgrID=E2.EID] Emp").unwrap();
    let result = eng.execute(&expr).expect("query should succeed");

    assert_eq!(
        result.len(),
        3,
        "self-join should produce 3 manager-employee pairs"
    );
    for row in result.iter() {
        let manager_eid = &row[0]; // E2.EID — the manager
        let employee_mgrid = &row[9]; // Emp.MgrID — who the employee reports to
        assert_eq!(
            manager_eid, employee_mgrid,
            "manager's EID must equal the employee's MgrID"
        );
    }
}

#[test]
fn test_21_union_incompatible_schemas_is_error() {
    let mut eng = Engine::new();
    let mut r = Relation::new(["A", "B"]);
    r.push([Value::Int(1), Value::Int(2)])
        .expect("static test data is well-formed");
    eng.load("R", r);
    let mut s = Relation::new(["A", "C"]);
    s.push([Value::Int(1), Value::Int(2)])
        .expect("static test data is well-formed");
    eng.load("S", s);
    let expr = parse_query("R union S").unwrap();
    let err = eng.execute(&expr).unwrap_err();
    assert!(
        matches!(err, SemanticError::SchemaMismatch { .. }),
        "expected SchemaMismatch, got {err:?}"
    );
}

#[test]
fn test_22_comparing_int_to_string_is_type_error() {
    let mut eng = Engine::new();
    eng.load("R", employees());
    let expr = parse_query("select[Age>'30'](R)").unwrap();
    let err = eng.execute(&expr).unwrap_err();
    assert!(
        matches!(err, SemanticError::TypeError { .. }),
        "expected TypeError, got {err:?}"
    );
}

#[test]
fn test_23_project_removes_duplicates() {
    let mut eng = Engine::new();
    eng.load("Employees", employees());
    let expr = parse_query("project[DID](Employees)").unwrap();
    let result = eng.execute(&expr).expect("query should succeed");

    assert_eq!(result.schema(), ["DID"]);
    // 3 employees, but only 2 distinct DIDs (D1, D2)
    assert_eq!(
        result.len(),
        2,
        "projection must remove duplicate DIDs"
    );
    assert!(result.contains([Value::Str("D1".into())]));
    assert!(result.contains([Value::Str("D2".into())]));
}

#[test]
fn test_24_duplicate_projected_attribute_is_error() {
    let mut eng = Engine::new();
    eng.load("R", employees());
    let expr = parse_query("project[Name, Name](R)").unwrap();
    let err = eng.execute(&expr).unwrap_err();
    assert!(
        matches!(err, SemanticError::DuplicateProjectedAttribute { .. }),
        "expected DuplicateProjectedAttribute, got {err:?}"
    );
}

#[test]
fn test_25_empty_result_shows_schema_and_no_tuples() {
    let mut eng = Engine::new();
    eng.load("R", employees());
    let expr = parse_query("select[Age>100](R)").unwrap();
    let result = eng.execute(&expr).expect("query should succeed");

    assert_eq!(result.len(), 0, "no employees older than 100");
    assert_eq!(result.schema(), ["EID", "Name", "Age", "DID"]);

    // Display should produce schema + "(0 tuples)"
    let display = format!("{result}");
    assert!(
        display.contains("(0 tuples)"),
        "Display should show (0 tuples), got: {display}"
    );
}
