//! Section 7.2 — Grammar and precedence. Table rows #10–#17.
use radb::{parse_query, CompareOp, ParseError, Predicate, Query};

#[test]
fn test_10_union_minus_left_associative() {
    let expr = parse_query("A union B minus C").unwrap();
    match expr {
        Query::Minus { left, right } => {
            assert!(matches!(*right, Query::Variable(ref r) if r == "C"));
            match *left {
                Query::Union { .. } => {} // correct grouping
                other => panic!("expected (A union B) as left child, got {other:#?}"),
            }
        }
        other => panic!("expected Minus(Union(A, B), C), got {other:#?}"),
    }
}

#[test]
fn test_11_minus_minus_left_associative() {
    let expr = parse_query("A minus B minus C").unwrap();
    match expr {
        Query::Minus { left, right } => {
            assert!(matches!(*right, Query::Variable(ref r) if r == "C"));
            match *left {
                Query::Minus { .. } => {} // correct: (A minus B) minus C
                other => panic!("expected (A minus B) as left child, got {other:#?}"),
            }
        }
        other => panic!("expected Minus(Minus(A, B), C), got {other:#?}"),
    }
}

/// #11 — data proof:  (A minus B) minus C ≠ A minus (B minus C)
///
/// A = {1,2,3}  B = {2,3}  C = {3}
///   (A−B)−C = {1}−{3}   = {1}
///    A−(B−C) = {1,2,3}−{2} = {1,3}
///
/// These tests load the data into the engine and verify the results.
#[test]
fn test_11b_left_grouping_gives_different_answer() {
    use radb::{Engine, Relation, Value};

    let mut eng = Engine::new();
    let mut a = Relation::new(["x"]);
    for v in [1, 2, 3] {
        a.push([Value::Int(v)])
            .expect("static test data is well-formed");
    }
    eng.load("A", a);
    let mut b = Relation::new(["x"]);
    for v in [2, 3] {
        b.push([Value::Int(v)])
            .expect("static test data is well-formed");
    }
    eng.load("B", b);
    let mut c = Relation::new(["x"]);
    c.push([Value::Int(3)])
        .expect("static test data is well-formed");
    eng.load("C", c);

    // Left-assoc: (A minus B) minus C = {1}
    let expr = parse_query("A minus B minus C").unwrap();
    let left = eng.execute(&expr).expect("query should succeed");
    assert_eq!(left.len(), 1);
    assert!(left.contains([Value::Int(1)]));

    // Explicit right grouping: A minus (B minus C) = {1, 3}
    let expr = parse_query("A minus (B minus C)").unwrap();
    let right = eng.execute(&expr).expect("query should succeed");
    assert_eq!(right.len(), 2);
    assert!(right.contains([Value::Int(1)]));
    assert!(right.contains([Value::Int(3)]));
}

#[test]
fn test_12_not_and_or_precedence() {
    let expr = parse_query("select[not (a=1 and b=2) or c>3](R)").unwrap();
    match expr {
        Query::Select { predicate, .. } => match predicate {
            Predicate::Or(left, right) => {
                // left = not (a=1 and b=2)
                match *left {
                    Predicate::Not(inner) => match *inner {
                        Predicate::And(_, _) => {} // correct
                        other => panic!("expected And inside Not, got {other:#?}"),
                    },
                    other => panic!("expected Not, got {other:#?}"),
                }
                // right = c > 3
                match *right {
                    Predicate::Compare {
                        op: CompareOp::Gt, ..
                    } => {}
                    other => panic!("expected Gt comparison, got {other:#?}"),
                }
            }
            other => panic!("expected Or at top level, got {other:#?}"),
        },
        other => panic!("expected Select, got {other:#?}"),
    }
}

#[test]
fn test_13_and_binds_tighter_than_or() {
    let expr = parse_query("select[a=1 and b=2 or c=3](R)").unwrap();
    match expr {
        Query::Select { predicate, .. } => match predicate {
            Predicate::Or(left, right) => {
                assert!(
                    matches!(*left, Predicate::And(_, _)),
                    "left child should be And, got {left:#?}"
                );
                assert!(
                    matches!(*right, Predicate::Compare { .. }),
                    "right child should be Compare, got {right:#?}"
                );
            }
            other => panic!("expected Or at top level, got {other:#?}"),
        },
        other => panic!("expected Select, got {other:#?}"),
    }
}

#[test]
fn test_14_triple_nesting_order() {
    let expr = parse_query("project[Name](select[Age>30](select[DID='D1'](Employees)))").unwrap();
    // outermost: Project
    match expr {
        Query::Project { attrs, input } => {
            assert_eq!(attrs, vec!["Name".to_string()]);
            // next: Select(Age>30)
            match *input {
                Query::Select { predicate, input } => {
                    assert!(matches!(
                        predicate,
                        Predicate::Compare {
                            op: CompareOp::Gt,
                            ..
                        }
                    ));
                    // innermost: Select(DID='D1')
                    match *input {
                        Query::Select { predicate, input } => {
                            assert!(matches!(
                                predicate,
                                Predicate::Compare {
                                    op: CompareOp::Eq,
                                    ..
                                }
                            ));
                            assert!(matches!(*input, Query::Variable(ref r) if r == "Employees"));
                        }
                        other => panic!("expected inner Select, got {other:#?}"),
                    }
                }
                other => panic!("expected middle Select, got {other:#?}"),
            }
        }
        other => panic!("expected Project, got {other:#?}"),
    }
}

#[test]
fn test_15_explicit_parens_override() {
    let expr = parse_query("(A union B) minus (C intersect D)").unwrap();
    match expr {
        Query::Minus { left, right } => {
            match *left {
                Query::Union { .. } => {}
                other => panic!("expected Union on left, got {other:#?}"),
            }
            match *right {
                Query::Intersect { .. } => {}
                other => panic!("expected Intersect on right, got {other:#?}"),
            }
        }
        other => panic!("expected Minus at top, got {other:#?}"),
    }
}

#[test]
fn test_16_missing_closing_paren_is_syntax_error() {
    let err = parse_query("select[Age>30](R").unwrap_err();
    // The error should be a ParseError (not a panic) with a position.
    match err {
        ParseError::UnexpectedEof { .. } => {}
        other => panic!("expected a parse error mentioning missing paren, got {other:#?}"),
    }
    let msg = err.to_string();
    assert!(
        msg.contains("col") || msg.contains("line") || msg.contains("expected"),
        "error message should mention position or what was expected, got: {msg}"
    );
}

#[test]
fn test_17_empty_projection_is_syntax_error() {
    let err = parse_query("project[](R)").unwrap_err();
    match err {
        ParseError::EmptyProjectionList { .. } => {}
        other => panic!("expected EmptyProjectionList, got {other:#?}"),
    }
    let msg = err.to_string();
    assert!(
        msg.to_lowercase().contains("empty") || msg.to_lowercase().contains("attribute"),
        "error message should mention the empty list, got: {msg}"
    );
}
