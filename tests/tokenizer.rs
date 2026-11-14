//! Section 7.1 — SQL Tokenizer. Table rows #1-#9.
use radb::sql::error::Category;
use radb::{Keyword, SqlError, TokenKind, tokenize};

fn kinds(input: &str) -> Vec<TokenKind> {
    tokenize(input)
        .unwrap()
        .into_iter()
        .map(|t| t.kind)
        .collect()
}

#[test]
fn test_1_no_whitespace_still_tokenizes() {
    let got = kinds("SELECT a FROM R");
    let want = vec![
        TokenKind::Keyword(Keyword::Select),
        TokenKind::Ident("a".into()),
        TokenKind::Keyword(Keyword::From),
        TokenKind::Ident("R".into()),
        TokenKind::Eof,
    ];
    assert_eq!(got, want);
}

#[test]
fn test_2_whitespace_is_insignificant() {
    let compact = kinds("SELECT a FROM R");
    let spaced = kinds("SELECT   a   FROM   R");
    assert_eq!(compact, spaced);
}

#[test]
fn test_3_ge_is_one_token() {
    let got = kinds("SELECT a FROM R WHERE Age >= 30");
    assert!(got.contains(&TokenKind::Ge));
}

#[test]
fn test_4_gt_then_negative_int_no_invented_operator() {
    let got = kinds("SELECT a FROM R WHERE Age > -30");
    assert!(got.contains(&TokenKind::Gt));
    assert!(got.contains(&TokenKind::Int(-30)));
}

#[test]
fn test_5_paren_inside_string_literal() {
    let got = kinds("SELECT a FROM R WHERE Name = 'Bob)'");
    assert!(
        got.iter()
            .any(|t| matches!(t, TokenKind::Str(s) if s == "Bob)"))
    );
}

#[test]
fn test_6_comma_inside_string_literal() {
    let got = kinds("SELECT a FROM R WHERE Name = 'a,b'");
    assert!(
        got.iter()
            .any(|t| matches!(t, TokenKind::Str(s) if s == "a,b"))
    );
}

#[test]
fn test_7_doubled_quote_is_escaped_quote() {
    let got = kinds("SELECT a FROM R WHERE Name = 'O''Brien'");
    assert!(
        got.iter()
            .any(|t| matches!(t, TokenKind::Str(s) if s == "O'Brien"))
    );
}

#[test]
fn test_8_keyword_spelled_attribute_lexes_as_keyword() {
    let got = kinds("SELECT union FROM R WHERE union = 3");
    assert!(
        got.iter()
            .any(|t| matches!(t, TokenKind::Keyword(Keyword::Union)))
    );
}

#[test]
fn test_9_unterminated_string_is_lex_error_with_position() {
    let err = tokenize("SELECT a FROM R WHERE Name = 'Bob").unwrap_err();
    match err {
        SqlError {
            category: Category::Lexical,
            ..
        } => {
            assert_eq!(err.at.line, 1);
        }
        other => panic!("expected SqlError::Lexical, got {other:?}"),
    }
    let msg = &tokenize("SELECT a FROM R WHERE Name = 'Bob")
        .unwrap_err()
        .to_string();
    assert!(
        msg.to_lowercase().contains("unterminated") || msg.to_lowercase().contains("string"),
        "error message should mention the unterminated string, got: {msg}"
    );
}
