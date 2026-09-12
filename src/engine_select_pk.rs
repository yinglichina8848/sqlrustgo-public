//! PK equality fast-path helper for `execute_select`.
//!
//! v4.1.0 / Phase A: Without this, sysbench's `WHERE id = ?` hot path
//! causes `FileStorage::scan` to clone the entire 10000-row table on
//! every query (8 threads × 8 queries/tx = 20 GB/s of allocator
//! traffic). This module exports a single helper that detects the
//! simple `pk_col = literal` pattern in a WHERE clause and returns
//! the column name + literal value. The engine then calls
//! `StorageEngine::scan_pk_eq` to read at most one row.
//!
//! This is a deliberately conservative first cut. We only take the
//! fast path when:
//! - the WHERE is a single conjunctive clause
//! - the clause is `col = literal` or `literal = col`
//!
//! Anything more complex (AND/OR chains, IN, BETWEEN, expressions)
//! falls through to the default `scan_with_ahi` path, which is
//! unchanged.

use sqlrustgo_parser::Expression;
use sqlrustgo_types::Value;

/// If `where_expr` is a single `col = literal` (or its symmetric
/// `literal = col`), return `(column_name, literal_value)`.
/// Otherwise return `None`.
///
/// We require the WHERE to be a single conjunctive clause. AND/OR
/// chains, IN, BETWEEN, etc. all return `None`.
pub fn try_extract_pk_eq(where_expr: &Expression) -> Option<(String, Value)> {
    extract_eq(where_expr)
}

fn extract_eq(expr: &Expression) -> Option<(String, Value)> {
    match expr {
        Expression::BinaryOp(left, op, right) => {
            if op.eq_ignore_ascii_case("=") {
                match (left.as_ref(), right.as_ref()) {
                    (Expression::Identifier(col), literal) => {
                        literal_to_value(literal).map(|v| (col.clone(), v))
                    }
                    (literal, Expression::Identifier(col)) => {
                        literal_to_value(literal).map(|v| (col.clone(), v))
                    }
                    _ => None,
                }
            } else {
                None
            }
        }
        _ => None,
    }
}

fn literal_to_value(expr: &Expression) -> Option<Value> {
    match expr {
        Expression::Literal(s) => parse_literal(s),
        Expression::UnaryOp(op, inner) => {
            let v = literal_to_value(inner)?;
            if op == "-" {
                negate(v)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn parse_literal(s: &str) -> Option<Value> {
    // Try integer first (most common for sysbench `id`).
    if let Ok(n) = s.parse::<i64>() {
        return Some(Value::Integer(n));
    }
    // Float
    if let Ok(f) = s.parse::<f64>() {
        return Some(Value::Float(f));
    }
    // Quoted string
    if s.len() >= 2 && ((s.starts_with('\'') && s.ends_with('\''))
        || (s.starts_with('"') && s.ends_with('"')))
    {
        return Some(Value::Text(s[1..s.len() - 1].to_string()));
    }
    // Booleans
    if s.eq_ignore_ascii_case("TRUE") {
        return Some(Value::Boolean(true));
    }
    if s.eq_ignore_ascii_case("FALSE") {
        return Some(Value::Boolean(false));
    }
    if s.eq_ignore_ascii_case("NULL") {
        return Some(Value::Null);
    }
    None
}

fn negate(v: Value) -> Option<Value> {
    match v {
        Value::Integer(n) => Some(Value::Integer(-n)),
        Value::Float(f) => Some(Value::Float(-f)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_pk_eq_int() {
        // `id = 42`
        let expr = Expression::BinaryOp(
            Box::new(Expression::Identifier("id".to_string())),
            "=".to_string(),
            Box::new(Expression::Literal("42".to_string())),
        );
        let r = try_extract_pk_eq(&expr);
        assert_eq!(r, Some(("id".to_string(), Value::Integer(42))));
    }

    #[test]
    fn extract_pk_eq_literal_left() {
        // `42 = id`
        let expr = Expression::BinaryOp(
            Box::new(Expression::Literal("42".to_string())),
            "=".to_string(),
            Box::new(Expression::Identifier("id".to_string())),
        );
        let r = try_extract_pk_eq(&expr);
        assert_eq!(r, Some(("id".to_string(), Value::Integer(42))));
    }

    #[test]
    fn extract_pk_eq_string_literal() {
        // `name = 'foo'`
        let expr = Expression::BinaryOp(
            Box::new(Expression::Identifier("name".to_string())),
            "=".to_string(),
            Box::new(Expression::Literal("'foo'".to_string())),
        );
        let r = try_extract_pk_eq(&expr);
        assert_eq!(
            r,
            Some(("name".to_string(), Value::Text("foo".to_string())))
        );
    }

    #[test]
    fn extract_pk_eq_negative() {
        // `id = -5`
        let expr = Expression::BinaryOp(
            Box::new(Expression::Identifier("id".to_string())),
            "=".to_string(),
            Box::new(Expression::UnaryOp(
                "-".to_string(),
                Box::new(Expression::Literal("5".to_string())),
            )),
        );
        let r = try_extract_pk_eq(&expr);
        assert_eq!(r, Some(("id".to_string(), Value::Integer(-5))));
    }

    #[test]
    fn reject_other_operators() {
        let expr = Expression::BinaryOp(
            Box::new(Expression::Identifier("id".to_string())),
            ">".to_string(),
            Box::new(Expression::Literal("42".to_string())),
        );
        assert_eq!(try_extract_pk_eq(&expr), None);
    }

    #[test]
    fn reject_identifier_identifier() {
        // `a = b`
        let expr = Expression::BinaryOp(
            Box::new(Expression::Identifier("a".to_string())),
            "=".to_string(),
            Box::new(Expression::Identifier("b".to_string())),
        );
        assert_eq!(try_extract_pk_eq(&expr), None);
    }

    #[test]
    fn reject_and_chain() {
        // `id = 1 AND name = 'x'` — not a single-PK-eq, fall through.
        let expr = Expression::BinaryOp(
            Box::new(Expression::BinaryOp(
                Box::new(Expression::Identifier("id".to_string())),
                "=".to_string(),
                Box::new(Expression::Literal("1".to_string())),
            )),
            "AND".to_string(),
            Box::new(Expression::BinaryOp(
                Box::new(Expression::Identifier("name".to_string())),
                "=".to_string(),
                Box::new(Expression::Literal("'x'".to_string())),
            )),
        );
        assert_eq!(try_extract_pk_eq(&expr), None);
    }
}
