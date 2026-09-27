//! Section 7.3 — Semantics. Table rows #18–#25.
use radb::{parse_query, Engine, Relation, SemanticError, Value};

/// Build the canonical relation from Section 4.1:
///
///   Employees (EID, Name, Age, DID) =
///     E1, John, 32, D1
///     E2, Alice, 28, D2
///     E3, Bob, 29, D1
fn employees() -> Relation {
    let mut rel = Relation::new(["EID", "Name", "Age", "DID"]).expect("test header names are distinct");
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
        rel.insert(row).expect("static test data is well-formed");
    }
    rel
}

/// A small departments relation to pair with Employees for join tests.
///
///   Departments (DID, DName) =
///     D1, Engineering
///     D2, Sales
fn departments() -> Relation {
    let mut rel = Relation::new(["DID", "DName"]).expect("test header names are distinct");
    for row in [
        [Value::Str("D1".into()), Value::Str("Engineering".into())],
        [Value::Str("D2".into()), Value::Str("Sales".into())],
    ] {
        rel.insert(row).expect("static test data is well-formed");
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
    let mut rel = Relation::new(["EID", "Name", "Age", "DID", "MgrID"]).expect("test header names are distinct");
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
        rel.insert(row).expect("static test data is well-formed");
    }
    rel
}

#[test]
fn test_18_select_attribute_vs_attribute() {
    let mut eng = Engine::new();
    let mut r = Relation::new(["A", "B"]).expect("test header names are distinct");
    for row in [
        [Value::Int(1), Value::Int(1)],
        [Value::Int(1), Value::Int(2)],
        [Value::Int(2), Value::Int(2)],
    ] {
        r.insert(row).expect("static test data is well-formed");
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
    let mut r = Relation::new(["A", "B"]).expect("test header names are distinct");
    r.insert([Value::Int(1), Value::Int(2)])
        .expect("static test data is well-formed");
    eng.load("R", r);
    let mut s = Relation::new(["A", "C"]).expect("test header names are distinct");
    s.insert([Value::Int(1), Value::Int(2)])
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

#[test]
fn test_26_column_named_like_its_relation_is_still_qualified() {
    // A column whose name is the same as its relation's is not a special
    // case: §4.3 qualifies *all* attributes, so K's column `K` becomes
    // `K.K`. Leaving it bare would also make the column unreachable, since
    // a condition could only spell it `K`, which is ambiguous with the
    // relation name.
    let mut eng = Engine::new();
    let mut k = Relation::new(["K"]).expect("test header names are distinct");
    k.insert([Value::Int(1)]).unwrap();
    eng.load("K", k);
    eng.load("R", Relation::new(["a", "b"]).expect("test header names are distinct"));

    let expr = parse_query("K times R").unwrap();
    let result = eng.execute(&expr).expect("query should succeed");
    assert_eq!(result.schema(), ["K.K", "R.a", "R.b"]);

    // And it is usable: K.K resolves in a condition, where bare `K` would not
    // have been expressible once the schema was qualified.
    let expr = parse_query("K join[K.K=1](R)").unwrap();
    let result = eng.execute(&expr).expect("K.K should resolve in a condition");
    assert_eq!(result.schema(), ["K.K", "R.a", "R.b"]);
}

#[test]
fn test_27_header_beginning_with_its_relation_name_is_still_qualified() {
    // `Rab` in relation `R` merely *starts with* the string "R"; it is not
    // qualified by it. §4.3 qualifies every attribute, so the result is
    // `R.Rab`. The idempotence rule that lets a `times`/`join` result skip
    // re-qualifying has to test for the qualifier *followed by a dot* — a bare
    // prefix test would leave `Rab` unqualified, and the column would then be
    // unreachable as `R.Rab`.
    let mut eng = Engine::new();
    let mut r = Relation::new(["Rab", "x"]).expect("test header names are distinct");
    r.insert([Value::Int(1), Value::Int(2)]).unwrap();
    eng.load("R", r);
    eng.load("S", Relation::new(["a"]).expect("test header names are distinct"));

    let result = eng
        .execute(&parse_query("R times S").unwrap())
        .expect("query should succeed");
    assert_eq!(result.schema(), ["R.Rab", "R.x", "S.a"]);

    // And R.Rab is addressable, which is the point of qualifying it.
    let result = eng
        .execute(&parse_query("R join[R.Rab=1](S)").unwrap())
        .expect("R.Rab should resolve in a condition");
    assert_eq!(result.schema(), ["R.Rab", "R.x", "S.a"]);
}

#[test]
fn test_28_attribute_name_must_match_a_whole_column_name() {
    // A name matches a column by being equal to it. So `ID` is not `DID` —
    // only a substring of it — and `select[ID=...]` is an unknown attribute
    // rather than a selection on `DID`.
    let mut eng = Engine::new();
    let mut t = Relation::new(["DID"]).expect("test header names are distinct");
    t.insert([Value::Int(7)]).unwrap();
    eng.load("T", t);
    eng.load("R", employees());

    for query in ["select[ID=7](T)", "project[ID](T)"] {
        match eng.execute(&parse_query(query).unwrap()) {
            Err(SemanticError::UnknownAttribute { name }) => {
                assert_eq!(name, "ID", "for {query}")
            }
            other => panic!("expected UnknownAttribute for {query}, got {other:?}"),
        }
    }

    // The whole-name forms still work, on a bare and on a qualified column.
    eng.execute(&parse_query("select[DID=7](T)").unwrap())
        .expect("DID should resolve against a bare column");
    eng.execute(&parse_query("select[Emp.DID='D1'](rename[Emp](R))").unwrap())
        .expect("Emp.DID should resolve against a qualified column");

    // A bare name does not reach a qualified column either. §4.3 makes a
    // `times`/`join` output's attributes the qualified names, so on this schema
    // the attributes are `U.EID`, `U.DID`, `U2.EID`, `U2.DID` and none of them
    // is called `DID` — a rule that let `DID` find `U.DID` would be inventing
    // that name, and here it would also have to choose between two columns
    // without saying which. `project` included: it used to accept a bare name
    // here and emit a bare header into an otherwise qualified schema.
    eng.load("U", employees());
    let mixed = "U times rename[U2](U)";
    assert_eq!(
        eng.execute(&parse_query(mixed).unwrap())
            .expect("a self join with rename should succeed")
            .schema(),
        ["U.EID", "U.Name", "U.Age", "U.DID", "U2.EID", "U2.Name", "U2.Age", "U2.DID"]
    );

    for query in [
        "select[DID='D1']((U times rename[U2](U)))",
        "project[DID]((U times rename[U2](U)))",
        "U join[DID=U2.DID] rename[U2](U)",
    ] {
        match eng.execute(&parse_query(query).unwrap()) {
            Err(SemanticError::UnknownAttribute { name }) => {
                assert_eq!(name, "DID", "for {query}")
            }
            other => panic!("expected UnknownAttribute for {query}, got {other:?}"),
        }
    }

    // Qualifying is how you name one, and then it resolves.
    let result = eng
        .execute(&parse_query("U join[U.DID=U2.DID] rename[U2](U)").unwrap())
        .expect("qualified names should resolve on both sides");
    assert_eq!(result.len(), 5, "D1×D1 (2×2) plus D2×D2 (1×1)");
}

#[test]
fn test_29_rename_that_would_merge_two_columns_is_an_error() {
    // `rename` strips the last period off every header, which is what stops
    // stacked renames accumulating — but it also means two columns differing
    // only in relation name collapse onto one. `U times rename[U2](V)` has
    // `U.EID` and `U2.EID`; renaming that to `X` would leave two columns both
    // called `X.EID`, and §4.3 calls colliding names an error.
    //
    // Left alone this is not a cosmetic problem. The columns hold different
    // values, and a condition on the shared name resolves to whichever comes
    // first, so `select[X.EID=2]` reads the `1` in column 1, finds no match,
    // and returns nothing for a row that is plainly there. Catching it in
    // `rename` puts the error on the operator that caused it.
    let mut eng = Engine::new();
    let mut u = Relation::new(["EID", "DID"]).expect("test header names are distinct");
    u.insert([Value::Int(1), Value::Int(10)]).unwrap();
    let mut v = Relation::new(["EID", "DID"]).expect("test header names are distinct");
    v.insert([Value::Int(2), Value::Int(20)]).unwrap();
    eng.load("U", u);
    eng.load("V", v);

    let expr = parse_query("rename[X]((U times rename[U2](V)))").unwrap();
    match eng.execute(&expr) {
        Err(SemanticError::SchemaMismatch { detail }) => {
            assert!(detail.contains("X.EID"), "names the collision, got: {detail}")
        }
        other => panic!("expected SchemaMismatch, got {other:?}"),
    }

    // Renames that do not collide are unaffected, including the two shapes
    // that exist to make names work: stacking, and giving a self join a second
    // name for one side.
    let result = eng
        .execute(&parse_query("rename[P](rename[Q](U))").unwrap())
        .expect("stacked renames should succeed");
    assert_eq!(result.schema(), ["P.EID", "P.DID"]);

    let result = eng
        .execute(&parse_query("U join[U.DID=U2.DID] rename[U2](U)").unwrap())
        .expect("renaming one side of a self join should succeed");
    assert_eq!(result.schema(), ["U.EID", "U.DID", "U2.EID", "U2.DID"]);
}
