//! Section 7.1 — Tokenizer. Table rows #1-#9.
use radb::{tokenize, Keyword, LexError, TokenKind};

#[test]
fn test_1_no_whitespace_still_tokenizes() {
    let got = tokenize("select[x1=3](R)").unwrap();
    let want = vec![
        TokenKind::Keyword(Keyword::Select),
        TokenKind::LBracket,
        TokenKind::Ident("x1".into()),
        TokenKind::Eq,
        TokenKind::Int(3),
        TokenKind::RBracket,
        TokenKind::LParen,
        TokenKind::Ident("R".into()),
        TokenKind::RParen,
        TokenKind::Eof,
    ];
    assert_eq!(got, want);
}

#[test]
fn test_2_whitespace_is_insignificant() {
    let compact = tokenize("select[x1=3](R)").unwrap();
    let spaced = tokenize("select[ x1 = 3 ](R)").unwrap();
    assert_eq!(compact, spaced);
}

#[test]
fn test_3_ge_is_one_token() {
    let got = tokenize("select[Age>=30](R)").unwrap();
    let want = vec![
        TokenKind::Keyword(Keyword::Select),
        TokenKind::LBracket,
        TokenKind::Ident("Age".into()),
        TokenKind::Ge,
        TokenKind::Int(30),
        TokenKind::RBracket,
        TokenKind::LParen,
        TokenKind::Ident("R".into()),
        TokenKind::RParen,
        TokenKind::Eof,
    ];
    assert_eq!(got, want);
}

#[test]
fn test_4_gt_then_negative_int_no_invented_operator() {
    let got = tokenize("select[Age>-30](R)").unwrap();
    let want = vec![
        TokenKind::Keyword(Keyword::Select),
        TokenKind::LBracket,
        TokenKind::Ident("Age".into()),
        TokenKind::Gt,
        TokenKind::Int(-30),
        TokenKind::RBracket,
        TokenKind::LParen,
        TokenKind::Ident("R".into()),
        TokenKind::RParen,
        TokenKind::Eof,
    ];
    assert_eq!(got, want);
}

#[test]
fn test_5_paren_inside_string_literal() {
    let got = tokenize("select[Name='Bob)'](R)").unwrap();
    let want = vec![
        TokenKind::Keyword(Keyword::Select),
        TokenKind::LBracket,
        TokenKind::Ident("Name".into()),
        TokenKind::Eq,
        TokenKind::Str("Bob)".into()),
        TokenKind::RBracket,
        TokenKind::LParen,
        TokenKind::Ident("R".into()),
        TokenKind::RParen,
        TokenKind::Eof,
    ];
    assert_eq!(got, want);
}

#[test]
fn test_6_comma_inside_string_literal() {
    let got = tokenize("select[Name='a,b'](R)").unwrap();
    let want = vec![
        TokenKind::Keyword(Keyword::Select),
        TokenKind::LBracket,
        TokenKind::Ident("Name".into()),
        TokenKind::Eq,
        TokenKind::Str("a,b".into()),
        TokenKind::RBracket,
        TokenKind::LParen,
        TokenKind::Ident("R".into()),
        TokenKind::RParen,
        TokenKind::Eof,
    ];
    assert_eq!(got, want);
}

#[test]
fn test_7_doubled_quote_is_escaped_quote() {
    let got = tokenize("select[Name='O''Brien'](R)").unwrap();
    let want = vec![
        TokenKind::Keyword(Keyword::Select),
        TokenKind::LBracket,
        TokenKind::Ident("Name".into()),
        TokenKind::Eq,
        TokenKind::Str("O'Brien".into()),
        TokenKind::RBracket,
        TokenKind::LParen,
        TokenKind::Ident("R".into()),
        TokenKind::RParen,
        TokenKind::Eof,
    ];
    assert_eq!(got, want);
}

#[test]
fn test_8_keyword_spelled_attribute_lexes_as_keyword() {
    let got = tokenize("select[union=3](R)").unwrap();
    let want = vec![
        TokenKind::Keyword(Keyword::Select),
        TokenKind::LBracket,
        TokenKind::Keyword(Keyword::Union),
        TokenKind::Eq,
        TokenKind::Int(3),
        TokenKind::RBracket,
        TokenKind::LParen,
        TokenKind::Ident("R".into()),
        TokenKind::RParen,
        TokenKind::Eof,
    ];
    assert_eq!(got, want);
}

#[test]
fn test_9_unterminated_string_is_lex_error_with_position() {
    let err = tokenize("select[Name='Bob](R)").unwrap_err();
    match err {
        LexError::UnterminatedString { at } => {
            assert_eq!(at.line, 1, "query is a single line");
        }
        other => panic!("expected LexError::UnterminatedString, got {other:?}"),
    }
    // And the Display impl must actually say something, not just Debug.
    let msg = &tokenize("select[Name='Bob](R)").unwrap_err().to_string();
    assert!(
        msg.to_lowercase().contains("unterminated") || msg.to_lowercase().contains("string"),
        "error message should mention the unterminated string, got: {msg}"
    );
}
