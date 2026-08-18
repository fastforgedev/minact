//! Expression types for workflow condition evaluation.
//!
//! Supports `${{ }}` expressions similar to GitHub Actions:
//! - Literals: `${{ 42 }}`, `${{ true }}`, `${{ 'hello' }}`
//! - Variables: `${{ github.ref }}`, `${{ env.FOO }}`, `${{ secrets.KEY }}`
//! - Functions: `${{ contains(github.ref, 'main') }}`, `${{ startsWith(github.ref, 'refs/tags/') }}`
//! - Ternary: `${{ condition && 'then' || 'else' }}`
//! - Pipeline: `${{ value | function }}`
//!
//! Also provides the expression evaluator that evaluates parsed expression trees
//! against a runtime context.

use crate::types::WorkflowError;
use crate::types::{Context, Value};

// ---------------------------------------------------------------------------
// AST types & parser
// ---------------------------------------------------------------------------

/// A parsed expression node tree.
#[derive(Debug, Clone, PartialEq)]
pub enum ExprNode {
    /// A literal value (string, number, boolean, null)
    Literal(ExprValue),
    /// A variable reference like `github.ref`, `env.FOO`, `secrets.KEY`
    Var(VarRef),
    /// A function call like `contains(github.ref, 'main')`
    Func(FuncCall),
    /// A ternary/conditional expression: `condition ? then : else`
    Ternary {
        condition: Box<ExprNode>,
        then_branch: Box<ExprNode>,
        else_branch: Box<ExprNode>,
    },
    /// Pipeline: `value | function`
    Pipeline {
        left: Box<ExprNode>,
        right: Box<ExprNode>,
    },
    /// Logical operators: &&, ||
    Logical {
        op: LogicalOp,
        left: Box<ExprNode>,
        right: Box<ExprNode>,
    },
    /// Comparison operators: ==, !=, <, >, <=, >=
    Comparison {
        op: ComparisonOp,
        left: Box<ExprNode>,
        right: Box<ExprNode>,
    },
    /// Unary operators: !
    Unary { op: UnaryOp, expr: Box<ExprNode> },
    /// Property or index access on the result of another expression, as in
    /// `fromJSON(needs.build.outputs.spec).include` or `github.event.commits[0]`.
    ///
    /// Namespaced references like `github.ref` are a [`Var`](ExprNode::Var)
    /// rather than this: their path is known while parsing. This is for
    /// reaching into a value that only exists once something has been
    /// evaluated.
    Access {
        target: Box<ExprNode>,
        accessor: Accessor,
    },
}

/// One step of an [`Access`](ExprNode::Access) chain.
#[derive(Debug, Clone, PartialEq)]
pub enum Accessor {
    /// `.name`, and `.*` which maps over an array.
    Property(String),
    /// `[expr]`, where the expression is an index or a key.
    Index(Box<ExprNode>),
}

/// Available literal value types.
#[derive(Debug, Clone, PartialEq)]
pub enum ExprValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
}

/// A variable reference with namespace and key path.
#[derive(Debug, Clone, PartialEq)]
pub struct VarRef {
    /// The namespace (github, env, secrets, jobs, steps, runner, inputs)
    pub namespace: String,
    /// The rest of the path segments
    pub path: Vec<String>,
}

/// A function call expression.
#[derive(Debug, Clone, PartialEq)]
pub struct FuncCall {
    pub name: String,
    pub args: Vec<ExprNode>,
}

/// Logical operators.
#[derive(Debug, Clone, PartialEq)]
pub enum LogicalOp {
    And,
    Or,
}

/// Comparison operators.
#[derive(Debug, Clone, PartialEq)]
pub enum ComparisonOp {
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
}

/// Unary operators.
#[derive(Debug, Clone, PartialEq)]
pub enum UnaryOp {
    Not,
}

/// Expression-related errors.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ExprError {
    #[error("Unclosed expression")]
    UnclosedExpression,

    #[error("Invalid syntax: {0}")]
    InvalidSyntax(String),

    #[error("Unknown function: {0}")]
    UnknownFunction(String),

    #[error("Unknown variable: {0}")]
    UnknownVariable(String),

    #[error("Type mismatch: {0}")]
    TypeMismatch(String),

    #[error("Division by zero")]
    DivisionByZero,
}

/// Context namespaces the parser recognises as variable references.
///
/// This must stay in step with `evaluate_var`; an identifier missing here
/// fails to parse, and the caller then falls back to the raw text — which is
/// how `${{ matrix.os }}` would silently reach the shell verbatim.
pub const KNOWN_NAMESPACES: &[&str] = &[
    "github", "env", "secrets", "needs", "steps", "runner", "inputs", "matrix", "strategy", "job",
    "vars",
];

// ---------------------------------------------------------------------------
// Recursive descent expression parser
// ---------------------------------------------------------------------------

struct Parser<'a> {
    input: &'a str,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            input: input.trim(),
            pos: 0,
        }
    }

    fn peek(&self) -> Option<char> {
        self.input[self.pos..].chars().next()
    }

    fn is_at_end(&self) -> bool {
        self.pos >= self.input.len()
    }

    fn advance(&mut self) -> Option<char> {
        let c = self.input[self.pos..].chars().next()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn skip_whitespace(&mut self) {
        while let Some(c) = self.peek() {
            if c.is_ascii_whitespace() {
                self.advance();
            } else {
                break;
            }
        }
    }

    fn parse(&mut self) -> Result<ExprNode, ExprError> {
        let expr = self.parse_or_expr()?;
        self.skip_whitespace();
        if !self.is_at_end() {
            let rest: String = self.input[self.pos..].chars().collect();
            return Err(ExprError::InvalidSyntax(format!(
                "Unexpected trailing content: {}",
                rest
            )));
        }
        Ok(expr)
    }

    // or_expr → and_expr ("||" and_expr)*
    fn parse_or_expr(&mut self) -> Result<ExprNode, ExprError> {
        let mut left = self.parse_and_expr()?;
        loop {
            self.skip_whitespace();
            if self.try_consume_str("||") {
                let right = self.parse_and_expr()?;
                left = ExprNode::Logical {
                    op: LogicalOp::Or,
                    left: Box::new(left),
                    right: Box::new(right),
                };
            } else {
                break;
            }
        }
        Ok(left)
    }

    // and_expr → comparison ("&&" comparison)*
    fn parse_and_expr(&mut self) -> Result<ExprNode, ExprError> {
        let mut left = self.parse_comparison()?;
        loop {
            self.skip_whitespace();
            if self.try_consume_str("&&") {
                let right = self.parse_comparison()?;
                left = ExprNode::Logical {
                    op: LogicalOp::And,
                    left: Box::new(left),
                    right: Box::new(right),
                };
            } else {
                break;
            }
        }
        Ok(left)
    }

    // comparison → primary (("==" | "!=" | "<" | ">" | "<=" | ">=") primary)?
    fn parse_comparison(&mut self) -> Result<ExprNode, ExprError> {
        let left = self.parse_primary()?;
        self.skip_whitespace();

        let op = if self.try_consume_str("==") {
            ComparisonOp::Eq
        } else if self.try_consume_str("!=") {
            ComparisonOp::Ne
        } else if self.try_consume_str("<=") {
            ComparisonOp::Le
        } else if self.try_consume_str(">=") {
            ComparisonOp::Ge
        } else if self.try_consume_str("<") {
            ComparisonOp::Lt
        } else if self.try_consume_str(">") {
            ComparisonOp::Gt
        } else {
            return Ok(left);
        };

        let right = self.parse_primary()?;
        Ok(ExprNode::Comparison {
            op,
            left: Box::new(left),
            right: Box::new(right),
        })
    }

    // primary → "(" expr ")" | "!" primary | literal | var_ref | func_call
    /// A primary expression plus any `.property` / `[index]` chain on it.
    fn parse_primary(&mut self) -> Result<ExprNode, ExprError> {
        let mut node = self.parse_atom()?;

        loop {
            // No `skip_whitespace` here: `a . b` is not an access chain, and
            // treating it as one would swallow the next token of a comparison.
            match self.peek() {
                Some('.') => {
                    self.advance();
                    let start = self.pos;
                    if self.peek() == Some('*') {
                        self.advance();
                    } else {
                        while let Some(c) = self.peek() {
                            if c.is_alphanumeric() || c == '_' || c == '-' {
                                self.advance();
                            } else {
                                break;
                            }
                        }
                    }
                    if start == self.pos {
                        return Err(ExprError::InvalidSyntax(
                            "Expected a property name after `.`".to_string(),
                        ));
                    }
                    node = ExprNode::Access {
                        target: Box::new(node),
                        accessor: Accessor::Property(self.input[start..self.pos].to_string()),
                    };
                }
                Some('[') => {
                    self.advance();
                    let index = self.parse_or_expr()?;
                    self.skip_whitespace();
                    if !self.try_consume_char(']') {
                        return Err(ExprError::InvalidSyntax("Expected ']'".to_string()));
                    }
                    node = ExprNode::Access {
                        target: Box::new(node),
                        accessor: Accessor::Index(Box::new(index)),
                    };
                }
                _ => break,
            }
        }

        Ok(node)
    }

    fn parse_atom(&mut self) -> Result<ExprNode, ExprError> {
        self.skip_whitespace();

        // Unary NOT
        if self.try_consume_str("!") {
            let expr = self.parse_primary()?;
            return Ok(ExprNode::Unary {
                op: UnaryOp::Not,
                expr: Box::new(expr),
            });
        }

        // Parenthesized expression
        if self.try_consume_char('(') {
            let expr = self.parse_or_expr()?;
            self.skip_whitespace();
            if !self.try_consume_char(')') {
                return Err(ExprError::InvalidSyntax("Expected ')'".to_string()));
            }
            return Ok(expr);
        }

        // Quoted string literal
        if self.peek() == Some('\'') || self.peek() == Some('"') {
            let quote = self.advance().unwrap();
            let mut s = String::new();
            loop {
                match self.advance() {
                    Some(c) if c == quote => break,
                    Some(c) => s.push(c),
                    None => {
                        return Err(ExprError::InvalidSyntax(
                            "Unclosed string literal".to_string(),
                        ))
                    }
                }
            }
            return Ok(ExprNode::Literal(ExprValue::String(s)));
        }

        // Read an identifier (word characters, dots for namespaced refs, hyphens)
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_alphanumeric() || c == '_' || c == '-' || c == '.' {
                self.advance();
            } else {
                break;
            }
        }

        let ident = &self.input[start..self.pos];
        if ident.is_empty() {
            return Err(ExprError::InvalidSyntax("Expected expression".to_string()));
        }

        self.skip_whitespace();

        // Function call: identifier ( ... )
        if self.peek() == Some('(') {
            let _ = self.advance(); // consume '('
            let mut args = Vec::new();
            let mut depth = 1;
            let arg_start = self.pos;

            while depth > 0 && !self.is_at_end() {
                if let Some(c) = self.advance() {
                    match c {
                        '(' => depth += 1,
                        ')' => {
                            depth -= 1;
                            if depth == 0 {
                                // Parse the arguments between matching parens
                                let args_str = &self.input[arg_start..self.pos - 1];
                                if !args_str.trim().is_empty() {
                                    for arg in split_args(args_str) {
                                        let mut arg_parser = Parser::new(arg);
                                        args.push(arg_parser.parse()?);
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }

            if depth != 0 {
                return Err(ExprError::UnclosedExpression);
            }

            return Ok(ExprNode::Func(FuncCall {
                name: ident.to_string(),
                args,
            }));
        }

        // Try as literal (numbers, booleans, null)
        if let Ok(lit) = parse_literal_value(ident) {
            return Ok(ExprNode::Literal(lit));
        }

        // Variable reference with namespace
        if let Some(dot) = ident.find('.') {
            let namespace = &ident[..dot];
            if KNOWN_NAMESPACES.contains(&namespace) {
                let path: Vec<String> =
                    ident[dot + 1..].split('.').map(|s| s.to_string()).collect();
                return Ok(ExprNode::Var(VarRef {
                    namespace: namespace.to_string(),
                    path,
                }));
            }
        }

        Err(ExprError::InvalidSyntax(format!(
            "Unknown identifier: {}",
            ident
        )))
    }

    fn try_consume_str(&mut self, s: &str) -> bool {
        self.skip_whitespace();
        if self.input[self.pos..].starts_with(s) {
            self.pos += s.len();
            true
        } else {
            false
        }
    }

    fn try_consume_char(&mut self, c: char) -> bool {
        self.skip_whitespace();
        if self.peek() == Some(c) {
            self.advance();
            true
        } else {
            false
        }
    }
}

fn parse_literal_value(input: &str) -> Result<ExprValue, ExprError> {
    let input = input.trim();
    if input.is_empty() {
        return Err(ExprError::InvalidSyntax("Empty literal".to_string()));
    }
    if input.eq_ignore_ascii_case("null") {
        return Ok(ExprValue::Null);
    }
    if let Ok(b) = input.parse::<bool>() {
        return Ok(ExprValue::Bool(b));
    }
    if let Ok(i) = input.parse::<i64>() {
        return Ok(ExprValue::Int(i));
    }
    if let Ok(f) = input.parse::<f64>() {
        return Ok(ExprValue::Float(f));
    }
    if (input.starts_with('\'') && input.ends_with('\''))
        || (input.starts_with('"') && input.ends_with('"'))
    {
        return Ok(ExprValue::String(input[1..input.len() - 1].to_string()));
    }
    Err(ExprError::InvalidSyntax(format!(
        "Not a literal: {}",
        input
    )))
}

/// Split function arguments by comma, respecting nested parentheses.
fn split_args(input: &str) -> Vec<&str> {
    let mut args = Vec::new();
    let mut depth = 0;
    let mut start = 0;
    // A comma only separates arguments at the top level and outside a string.
    // Without this, `join(list, ', ')` splits inside its own separator.
    let mut quote: Option<char> = None;

    for (i, c) in input.char_indices() {
        match c {
            _ if quote == Some(c) => quote = None,
            _ if quote.is_some() => {}
            '\'' | '"' => quote = Some(c),
            '(' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            ',' if depth == 0 => {
                args.push(&input[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }

    if start < input.len() {
        args.push(&input[start..]);
    }

    args
}

/// Parse an expression string (without the `${{ }}` wrapper).
pub fn parse_expression(input: &str) -> Result<ExprNode, ExprError> {
    let mut parser = Parser::new(input);
    parser.parse()
}

/// The set of built-in functions available in expressions.
#[derive(Debug, Clone)]
pub enum BuiltinFunction {
    Contains,
    StartsWith,
    EndsWith,
    Format,
    Join,
    ToJSON,
    FromJSON,
    HashFiles,
    Success,
    Failure,
    Always,
    Cancelled,
}

impl BuiltinFunction {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "contains" => Some(Self::Contains),
            "startsWith" => Some(Self::StartsWith),
            "endsWith" => Some(Self::EndsWith),
            "format" => Some(Self::Format),
            "join" => Some(Self::Join),
            "toJSON" => Some(Self::ToJSON),
            "fromJSON" => Some(Self::FromJSON),
            "hashFiles" => Some(Self::HashFiles),
            "success" => Some(Self::Success),
            "failure" => Some(Self::Failure),
            "always" => Some(Self::Always),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Expression evaluator
// ---------------------------------------------------------------------------

/// Evaluates an expression node against the provided context.
pub fn evaluate(expr: &ExprNode, ctx: &Context) -> Result<Value, WorkflowError> {
    match expr {
        ExprNode::Literal(val) => Ok(literal_to_value(val)),
        ExprNode::Var(var) => evaluate_var(var, ctx),
        ExprNode::Func(func) => evaluate_func(func, ctx),
        ExprNode::Ternary {
            condition,
            then_branch,
            else_branch,
        } => {
            let cond_val = evaluate(condition, ctx)?;
            let truthy = is_truthy(&cond_val);
            if truthy {
                evaluate(then_branch, ctx)
            } else {
                evaluate(else_branch, ctx)
            }
        }
        ExprNode::Access { target, accessor } => {
            let value = evaluate(target, ctx)?;
            let accessor = match accessor {
                Accessor::Property(name) => name.clone(),
                Accessor::Index(expr) => evaluate(expr, ctx)?.to_string(),
            };
            Ok(access_value(&value, &accessor))
        }
        ExprNode::Pipeline { left, right } => {
            let left_val = evaluate(left, ctx)?;
            // Right side should be a function that takes the left value
            if let ExprNode::Func(func) = right.as_ref() {
                evaluate_func_with_arg(func, &left_val, ctx)
            } else {
                Err(WorkflowError::ExpressionError(
                    "Pipeline right side must be a function".to_string(),
                ))
            }
        }
        ExprNode::Logical { op, left, right } => {
            let left_val = evaluate(left, ctx)?;
            match op {
                LogicalOp::And => {
                    if !is_truthy(&left_val) {
                        Ok(left_val)
                    } else {
                        evaluate(right, ctx)
                    }
                }
                LogicalOp::Or => {
                    if is_truthy(&left_val) {
                        Ok(left_val)
                    } else {
                        evaluate(right, ctx)
                    }
                }
            }
        }
        ExprNode::Comparison { op, left, right } => {
            let left_val = evaluate(left, ctx)?;
            let right_val = evaluate(right, ctx)?;
            evaluate_comparison(op, &left_val, &right_val)
        }
        ExprNode::Unary { op, expr } => {
            let val = evaluate(expr, ctx)?;
            match op {
                UnaryOp::Not => Ok(Value::Bool(!is_truthy(&val))),
            }
        }
    }
}

/// Evaluate an expression and coerce the result to a boolean.
pub fn evaluate_bool(expr: &ExprNode, ctx: &Context) -> Result<bool, WorkflowError> {
    let val = evaluate(expr, ctx)?;
    Ok(is_truthy(&val))
}

/// Evaluate a lone `${{ … }}` to a value rather than to text.
///
/// Interpolation flattens everything to a string, which loses the structure a
/// dynamic matrix is made of. This keeps it: `${{ fromJSON(x) }}` comes back
/// as the array or object it parsed to.
pub fn evaluate_value(source: &str, ctx: &Context) -> Result<Value, WorkflowError> {
    let trimmed = source.trim();
    let inner = trimmed
        .strip_prefix("${{")
        .and_then(|rest| rest.strip_suffix("}}"))
        .unwrap_or(trimmed);
    let parsed = parse_expression(inner.trim())
        .map_err(|e| WorkflowError::ExpressionError(format!("{}", e)))?;
    evaluate(&parsed, ctx)
}

/// Evaluate a string with expression interpolation.
/// Replaces all `${{ ... }}` occurrences with their evaluated values.
pub fn evaluate_string(input: &str, ctx: &Context) -> Result<String, WorkflowError> {
    let mut result = String::new();
    let mut rest = input;

    while let Some(start) = rest.find("${{") {
        // Add text before the expression
        result.push_str(&rest[..start]);
        let after_start = &rest[start + 3..];

        // Find the closing }}
        if let Some(end) = after_start.find("}}") {
            let expr_str = after_start[..end].trim();
            let parsed = parse_expression(expr_str)
                .map_err(|e| WorkflowError::ExpressionError(format!("{}", e)))?;
            let value = evaluate(&parsed, ctx)?;
            result.push_str(&value.to_string());
            rest = &after_start[end + 2..];
        } else {
            result.push_str(&rest[start..]);
            rest = "";
            break;
        }
    }

    result.push_str(rest);
    Ok(result)
}

/// Hash the set of files matching any of `patterns`, relative to `workspace`.
///
/// GitHub's definition: a SHA-256 per matched file, then a SHA-256 over those
/// digests. The set is sorted so the result does not depend on the order the
/// filesystem happened to hand paths back, and an empty set hashes to the
/// empty string — which is what makes `if: hashFiles(...) != ''` work.
pub fn hash_files(workspace: &std::path::Path, patterns: &[String]) -> String {
    use sha2::Digest;

    let mut matched: Vec<std::path::PathBuf> = Vec::new();
    for pattern in patterns {
        // Patterns are relative to the workspace; an absolute one is its own.
        let joined = if std::path::Path::new(pattern).is_absolute() {
            pattern.clone()
        } else {
            workspace.join(pattern).to_string_lossy().to_string()
        };
        let Ok(entries) = glob::glob(&joined) else {
            continue;
        };
        for entry in entries.flatten() {
            if entry.is_file() && !matched.contains(&entry) {
                matched.push(entry);
            }
        }
    }

    if matched.is_empty() {
        return String::new();
    }
    matched.sort();

    let mut outer = sha2::Sha256::new();
    for path in matched {
        let Ok(contents) = std::fs::read(&path) else {
            continue;
        };
        let mut inner = sha2::Sha256::new();
        inner.update(&contents);
        outer.update(inner.finalize());
    }
    format!("{:x}", outer.finalize())
}

/// Reach into a value by property name or index.
///
/// Anything that does not resolve reads as null rather than aborting, which is
/// how GitHub treats a missing property.
fn access_value(value: &Value, accessor: &str) -> Value {
    match value {
        Value::Map(map) => map.get(accessor).cloned().unwrap_or(Value::Null),
        Value::Array(items) => {
            // `*` is the splat: `commits.*.message` is every commit's message.
            if accessor == "*" {
                return Value::Array(items.clone());
            }
            if let Ok(index) = accessor.parse::<usize>() {
                return items.get(index).cloned().unwrap_or(Value::Null);
            }
            // A property name applied to an array maps over it, which is what
            // makes the second half of `commits.*.message` work.
            Value::Array(
                items
                    .iter()
                    .map(|item| access_value(item, accessor))
                    .collect(),
            )
        }
        _ => Value::Null,
    }
}

/// Convert parsed JSON into an expression value.
fn value_from_json(json: &serde_json::Value) -> Value {
    match json {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => match (n.as_i64(), n.as_f64()) {
            (Some(i), _) => Value::Int(i),
            (None, Some(f)) => Value::Float(f),
            _ => Value::String(n.to_string()),
        },
        serde_json::Value::String(s) => Value::String(s.clone()),
        serde_json::Value::Array(items) => {
            Value::Array(items.iter().map(value_from_json).collect())
        }
        serde_json::Value::Object(fields) => Value::Map(
            fields
                .iter()
                .map(|(key, val)| (key.clone(), value_from_json(val)))
                .collect(),
        ),
    }
}

/// Convert an expression value back to JSON.
pub fn value_to_json(value: &Value) -> serde_json::Value {
    match value {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Int(i) => serde_json::Value::Number((*i).into()),
        Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Value::String(s) => serde_json::Value::String(s.clone()),
        Value::Array(items) => serde_json::Value::Array(items.iter().map(value_to_json).collect()),
        // `serde_json::Map` is a `BTreeMap` by default, so the keys come out
        // sorted and `toJSON` is the same string from run to run.
        Value::Map(fields) => serde_json::Value::Object(
            fields
                .iter()
                .map(|(key, val)| (key.clone(), value_to_json(val)))
                .collect(),
        ),
    }
}

fn literal_to_value(val: &ExprValue) -> Value {
    match val {
        ExprValue::Null => Value::Null,
        ExprValue::Bool(b) => Value::Bool(*b),
        ExprValue::Int(i) => Value::Int(*i),
        ExprValue::Float(f) => Value::Float(*f),
        ExprValue::String(s) => Value::String(s.clone()),
    }
}

fn evaluate_var(var: &VarRef, ctx: &Context) -> Result<Value, WorkflowError> {
    match var.namespace.as_str() {
        "github" => evaluate_github_var(&var.path, ctx),
        "env" => evaluate_env_var(&var.path, ctx),
        "secrets" => evaluate_secrets_var(&var.path, ctx),
        "needs" => evaluate_jobs_var(&var.path, ctx),
        "steps" => evaluate_steps_var(&var.path, ctx),
        "runner" => evaluate_runner_var(&var.path, ctx),
        "inputs" => evaluate_inputs_var(&var.path, ctx),
        "matrix" => evaluate_matrix_var(&var.path, ctx),
        "strategy" => evaluate_strategy_var(&var.path, ctx),
        "job" => evaluate_job_var(&var.path, ctx),
        // Repository/organisation variables have no local equivalent; they
        // read as empty rather than aborting the run.
        "vars" => Ok(Value::String(String::new())),
        _ => Err(WorkflowError::ExpressionError(format!(
            "Unknown variable namespace: {}",
            var.namespace
        ))),
    }
}

fn evaluate_job_var(path: &[String], ctx: &Context) -> Result<Value, WorkflowError> {
    if path.first().map(String::as_str) == Some("status") {
        let status = if ctx.status.failure {
            "failure"
        } else if ctx.status.cancelled {
            "cancelled"
        } else {
            "success"
        };
        return Ok(Value::String(status.to_string()));
    }
    Ok(Value::String(String::new()))
}

fn evaluate_matrix_var(path: &[String], ctx: &Context) -> Result<Value, WorkflowError> {
    if path.is_empty() {
        return Ok(Value::Map(ctx.matrix.clone()));
    }

    let Some(value) = ctx.matrix.get(&path[0]) else {
        return Ok(Value::String(String::new()));
    };

    // Matrix values can be structured, so `matrix.target.name` has to walk in.
    Ok(index_path(value, &path[1..]))
}

fn evaluate_strategy_var(path: &[String], ctx: &Context) -> Result<Value, WorkflowError> {
    if path.is_empty() {
        return Ok(Value::String(String::new()));
    }
    // GitHub spells these with dashes; accept underscores too.
    match path[0].replace('_', "-").as_str() {
        "fail-fast" => Ok(Value::Bool(ctx.strategy.fail_fast)),
        "job-index" => Ok(Value::Int(ctx.strategy.job_index as i64)),
        "job-total" => Ok(Value::Int(ctx.strategy.job_total as i64)),
        "max-parallel" => Ok(match ctx.strategy.max_parallel {
            Some(n) => Value::Int(n as i64),
            None => Value::Null,
        }),
        _ => Ok(Value::String(String::new())),
    }
}

/// Walk a dotted path into a structured value, yielding empty on a miss.
fn index_path(value: &Value, path: &[String]) -> Value {
    let mut current = value;
    for segment in path {
        match current {
            Value::Map(map) => match map.get(segment) {
                Some(next) => current = next,
                None => return Value::String(String::new()),
            },
            Value::Array(items) => match segment.parse::<usize>().ok().and_then(|i| items.get(i)) {
                Some(next) => current = next,
                None => return Value::String(String::new()),
            },
            _ => return Value::String(String::new()),
        }
    }
    current.clone()
}

fn evaluate_github_var(path: &[String], ctx: &Context) -> Result<Value, WorkflowError> {
    if path.is_empty() {
        return Ok(Value::String(ctx.github.event_name.clone()));
    }
    match path[0].as_str() {
        "event_name" => Ok(Value::String(ctx.github.event_name.clone())),
        // The payload is an object, so `github.event.pull_request.number`
        // has to walk into it rather than stringifying the whole thing.
        "event" => {
            let payload = Value::Map(
                ctx.github
                    .event
                    .iter()
                    .map(|(key, value)| (key.clone(), value_from_json(value)))
                    .collect(),
            );
            Ok(path[1..]
                .iter()
                .fold(payload, |value, key| access_value(&value, key)))
        }
        "repository" => Ok(Value::String(ctx.github.repository.clone())),
        "repository_owner" => Ok(Value::String(
            ctx.github
                .repository
                .split('/')
                .next()
                .unwrap_or_default()
                .to_string(),
        )),
        "ref" => Ok(Value::String(ctx.github.ref_name.clone())),
        // `ref_name` is the short form: `refs/heads/main` -> `main`.
        "ref_name" => Ok(Value::String(short_ref_name(&ctx.github.ref_name))),
        "sha" => Ok(Value::String(ctx.github.sha.clone())),
        "workspace" => Ok(Value::String(ctx.github.workspace.clone())),
        "action" => Ok(Value::String(ctx.github.action.clone())),
        "action_path" => Ok(Value::String(ctx.github.action_path.clone())),
        "action_repository" => Ok(Value::String(ctx.github.action_repository.clone())),
        "action_ref" => Ok(Value::String(ctx.github.action_ref.clone())),
        "workflow" => Ok(Value::String(ctx.github.workflow.clone())),
        "job" => Ok(Value::String(ctx.github.job.clone())),
        "run_id" => Ok(Value::String(ctx.github.run_id.clone())),
        "run_number" => Ok(Value::String(ctx.github.run_number.clone())),
        "run_attempt" => Ok(Value::String(ctx.github.run_attempt.clone())),
        "ref_type" => Ok(Value::String(ctx.github.ref_type.clone())),
        "ref_protected" => Ok(Value::Bool(ctx.github.ref_protected)),
        "base_ref" => Ok(Value::String(ctx.github.base_ref.clone())),
        "head_ref" => Ok(Value::String(ctx.github.head_ref.clone())),
        "server_url" => Ok(Value::String(ctx.github.server_url.clone())),
        "api_url" => Ok(Value::String(ctx.github.api_url.clone())),
        "graphql_url" => Ok(Value::String(ctx.github.graphql_url.clone())),
        "event_path" => Ok(Value::String(ctx.github.event_path.clone())),
        "token" => Ok(Value::String(ctx.github.token.clone())),
        "repositoryUrl" => Ok(Value::String(format!(
            "git://{}/{}.git",
            ctx.github
                .server_url
                .trim_start_matches("https://")
                .trim_start_matches("http://"),
            ctx.github.repository
        ))),
        "actor" | "triggering_actor" => Ok(Value::String(ctx.github.actor.clone())),
        // GitHub returns null (rendered as empty) for properties it does not
        // populate, so unknown properties must not abort the run.
        _ => Ok(Value::String(String::new())),
    }
}

/// Strip the `refs/heads/` or `refs/tags/` prefix from a git ref.
pub fn short_ref_name(git_ref: &str) -> String {
    git_ref
        .strip_prefix("refs/heads/")
        .or_else(|| git_ref.strip_prefix("refs/tags/"))
        .unwrap_or(git_ref)
        .to_string()
}

fn evaluate_env_var(path: &[String], ctx: &Context) -> Result<Value, WorkflowError> {
    if path.is_empty() {
        return Ok(Value::Map(
            ctx.env
                .iter()
                .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                .collect(),
        ));
    }
    let key = &path[0];
    Ok(match ctx.env.get(key) {
        Some(v) => Value::String(v.clone()),
        None => Value::String(String::new()),
    })
}

fn evaluate_secrets_var(path: &[String], ctx: &Context) -> Result<Value, WorkflowError> {
    if path.is_empty() {
        return Ok(Value::Map(std::collections::HashMap::new()));
    }
    let key = &path[0];
    Ok(match ctx.secrets.get(key) {
        Some(v) => Value::String(v.clone()),
        None => Value::String(String::new()),
    })
}

fn evaluate_jobs_var(path: &[String], ctx: &Context) -> Result<Value, WorkflowError> {
    if path.len() < 2 {
        return Err(WorkflowError::ExpressionError(
            "needs variable requires job_id and one of 'result' or 'outputs.<name>'".to_string(),
        ));
    }
    let job_id = &path[0];

    // `needs.<job_id>.result` — the dependency's conclusion.
    if path[1] == "result" {
        return Ok(match ctx.job_results.get(job_id) {
            Some(conclusion) => Value::String(conclusion.as_str().to_string()),
            None => Value::String(String::new()),
        });
    }

    // `needs.<job_id>.outputs.<name>`, and the shorthand `needs.<job_id>.<name>`.
    let output_name = if path[1] == "outputs" && path.len() > 2 {
        &path[2]
    } else {
        &path[1]
    };

    Ok(match ctx.job_outputs.get(job_id) {
        Some(outputs) => match outputs.get(output_name) {
            Some(v) => Value::String(v.clone()),
            None => Value::String(String::new()),
        },
        None => Value::String(String::new()),
    })
}

fn evaluate_steps_var(path: &[String], ctx: &Context) -> Result<Value, WorkflowError> {
    if path.len() < 2 {
        return Err(WorkflowError::ExpressionError(
            "steps variable requires step_id and one of 'outcome', 'conclusion' or 'outputs.<name>'"
                .to_string(),
        ));
    }
    let step_id = &path[0];

    // `steps.<id>.outcome` (raw) and `steps.<id>.conclusion` (after
    // continue-on-error is applied).
    match path[1].as_str() {
        "outcome" => {
            return Ok(match ctx.step_status.get(step_id) {
                Some(status) => Value::String(status.outcome.as_str().to_string()),
                None => Value::String(String::new()),
            });
        }
        "conclusion" => {
            return Ok(match ctx.step_status.get(step_id) {
                Some(status) => Value::String(status.conclusion.as_str().to_string()),
                None => Value::String(String::new()),
            });
        }
        _ => {}
    }

    if path.len() < 3 {
        return Err(WorkflowError::ExpressionError(
            "steps variable requires step_id.outputs.key format".to_string(),
        ));
    }
    let output_name = &path[2];
    Ok(match ctx.step_outputs.get(step_id) {
        Some(outputs) => match outputs.get(output_name) {
            Some(v) => Value::String(v.clone()),
            None => Value::String(String::new()),
        },
        None => Value::String(String::new()),
    })
}

fn evaluate_runner_var(path: &[String], ctx: &Context) -> Result<Value, WorkflowError> {
    if path.is_empty() {
        return Ok(Value::String(ctx.runner.os.clone()));
    }
    match path[0].as_str() {
        "os" => Ok(Value::String(ctx.runner.os.clone())),
        "arch" => Ok(Value::String(ctx.runner.arch.clone())),
        "temp" => Ok(Value::String(ctx.runner.temp.clone())),
        "tool_cache" => Ok(Value::String(ctx.runner.tool_cache.clone())),
        // Unknown properties render as empty, matching GitHub.
        _ => Ok(Value::String(String::new())),
    }
}

fn evaluate_inputs_var(path: &[String], ctx: &Context) -> Result<Value, WorkflowError> {
    if path.is_empty() {
        return Ok(Value::Map(
            ctx.inputs
                .iter()
                .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                .collect(),
        ));
    }
    let key = &path[0];
    Ok(match ctx.inputs.get(key) {
        Some(v) => Value::String(v.clone()),
        None => Value::String(String::new()),
    })
}

fn evaluate_func(func: &FuncCall, ctx: &Context) -> Result<Value, WorkflowError> {
    evaluate_func_with_arg(func, &Value::Null, ctx)
}

fn evaluate_func_with_arg(
    func: &FuncCall,
    piped_arg: &Value,
    ctx: &Context,
) -> Result<Value, WorkflowError> {
    let builtin = BuiltinFunction::from_name(&func.name).ok_or_else(|| {
        WorkflowError::ExpressionError(format!("Unknown function: {}", func.name))
    })?;

    // Evaluate all arguments, inserting piped_arg as the first if it's not Null
    let mut args = Vec::new();
    if !matches!(piped_arg, Value::Null) {
        args.push(piped_arg.clone());
    }
    for arg in &func.args {
        args.push(evaluate(arg, ctx)?);
    }

    match builtin {
        BuiltinFunction::Contains => {
            if args.len() < 2 {
                return Err(WorkflowError::ExpressionError(
                    "contains requires at least 2 arguments".to_string(),
                ));
            }
            let haystack = args[0].to_string();
            let needle = args[1].to_string();
            Ok(Value::Bool(haystack.contains(&needle)))
        }
        BuiltinFunction::StartsWith => {
            if args.len() < 2 {
                return Err(WorkflowError::ExpressionError(
                    "startsWith requires 2 arguments".to_string(),
                ));
            }
            let s = args[0].to_string();
            let prefix = args[1].to_string();
            Ok(Value::Bool(s.starts_with(&prefix)))
        }
        BuiltinFunction::EndsWith => {
            if args.len() < 2 {
                return Err(WorkflowError::ExpressionError(
                    "endsWith requires 2 arguments".to_string(),
                ));
            }
            let s = args[0].to_string();
            let suffix = args[1].to_string();
            Ok(Value::Bool(s.ends_with(&suffix)))
        }
        BuiltinFunction::Format => {
            if args.is_empty() {
                return Err(WorkflowError::ExpressionError(
                    "format requires at least 1 argument".to_string(),
                ));
            }
            let template = args[0].to_string();
            let replacements: Vec<String> = args[1..].iter().map(|v| v.to_string()).collect();
            let mut result = template;
            for (i, r) in replacements.iter().enumerate() {
                result = result.replace(&format!("{{{}}}", i), r);
            }
            Ok(Value::String(result))
        }
        BuiltinFunction::Join => {
            if args.is_empty() {
                return Err(WorkflowError::ExpressionError(
                    "join requires arguments".to_string(),
                ));
            }
            // `join(array, optionalSeparator)`: the first argument is the
            // array whose *elements* are joined, not one of the values.
            let separator = match args.get(1) {
                Some(separator) => separator.to_string(),
                None => ",".to_string(),
            };
            let items: Vec<String> = match &args[0] {
                Value::Array(items) => items.iter().map(|item| item.to_string()).collect(),
                // GitHub returns a non-array argument as its own string.
                other => vec![other.to_string()],
            };
            Ok(Value::String(items.join(&separator)))
        }
        BuiltinFunction::ToJSON => {
            if args.is_empty() {
                return Err(WorkflowError::ExpressionError(
                    "toJSON requires 1 argument".to_string(),
                ));
            }
            // GitHub pretty-prints, and workflows depend on that: a matrix
            // rendered with `toJSON` shows up in logs this way.
            Ok(Value::String(
                serde_json::to_string_pretty(&value_to_json(&args[0]))
                    .unwrap_or_else(|_| "null".to_string()),
            ))
        }
        BuiltinFunction::FromJSON => {
            if args.is_empty() {
                return Err(WorkflowError::ExpressionError(
                    "fromJSON requires 1 argument".to_string(),
                ));
            }
            let source = args[0].to_string();
            let parsed: serde_json::Value = serde_json::from_str(&source).map_err(|e| {
                WorkflowError::ExpressionError(format!(
                    "fromJSON could not parse its argument: {}",
                    e
                ))
            })?;
            Ok(value_from_json(&parsed))
        }
        BuiltinFunction::HashFiles => {
            if args.is_empty() {
                return Err(WorkflowError::ExpressionError(
                    "hashFiles requires at least 1 argument".to_string(),
                ));
            }
            let patterns: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
            Ok(Value::String(hash_files(
                std::path::Path::new(&ctx.github.workspace),
                &patterns,
            )))
        }
        BuiltinFunction::Success => Ok(Value::Bool(ctx.status.success)),
        BuiltinFunction::Failure => Ok(Value::Bool(ctx.status.failure)),
        BuiltinFunction::Always => Ok(Value::Bool(true)),
        BuiltinFunction::Cancelled => Ok(Value::Bool(ctx.status.cancelled)),
    }
}

fn evaluate_comparison(
    op: &ComparisonOp,
    left: &Value,
    right: &Value,
) -> Result<Value, WorkflowError> {
    let result = match (left, right) {
        (Value::String(l), Value::String(r)) => match op {
            ComparisonOp::Eq => l == r,
            ComparisonOp::Ne => l != r,
            ComparisonOp::Lt => l < r,
            ComparisonOp::Gt => l > r,
            ComparisonOp::Le => l <= r,
            ComparisonOp::Ge => l >= r,
        },
        (Value::Int(l), Value::Int(r)) => match op {
            ComparisonOp::Eq => l == r,
            ComparisonOp::Ne => l != r,
            ComparisonOp::Lt => l < r,
            ComparisonOp::Gt => l > r,
            ComparisonOp::Le => l <= r,
            ComparisonOp::Ge => l >= r,
        },
        (Value::Float(l), Value::Float(r)) => match op {
            ComparisonOp::Eq => (l - r).abs() < f64::EPSILON,
            ComparisonOp::Ne => (l - r).abs() >= f64::EPSILON,
            ComparisonOp::Lt => l < r,
            ComparisonOp::Gt => l > r,
            ComparisonOp::Le => l <= r,
            ComparisonOp::Ge => l >= r,
        },
        (Value::Bool(l), Value::Bool(r)) => match op {
            ComparisonOp::Eq => l == r,
            ComparisonOp::Ne => l != r,
            _ => false,
        },
        // Mixed types: GitHub coerces to numbers when it can, so `4 > '3'`
        // and `1 == 1.0` behave arithmetically; otherwise compare as strings.
        _ => match (to_number(left), to_number(right)) {
            (Some(l), Some(r)) => match op {
                ComparisonOp::Eq => (l - r).abs() < f64::EPSILON,
                ComparisonOp::Ne => (l - r).abs() >= f64::EPSILON,
                ComparisonOp::Lt => l < r,
                ComparisonOp::Gt => l > r,
                ComparisonOp::Le => l <= r,
                ComparisonOp::Ge => l >= r,
            },
            _ => {
                let l = left.to_string();
                let r = right.to_string();
                match op {
                    ComparisonOp::Eq => l == r,
                    ComparisonOp::Ne => l != r,
                    ComparisonOp::Lt => l < r,
                    ComparisonOp::Gt => l > r,
                    ComparisonOp::Le => l <= r,
                    ComparisonOp::Ge => l >= r,
                }
            }
        },
    };
    Ok(Value::Bool(result))
}

/// Coerce a value to a number the way GitHub Actions does when comparing
/// operands of different types. Returns `None` when there is no sensible
/// numeric reading.
fn to_number(value: &Value) -> Option<f64> {
    match value {
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) => Some(*f),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        Value::Null => Some(0.0),
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                Some(0.0)
            } else {
                trimmed.parse::<f64>().ok()
            }
        }
        _ => None,
    }
}

/// GitHub Actions truthiness rules.
fn is_truthy(val: &Value) -> bool {
    match val {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Int(i) => *i != 0,
        Value::Float(f) => *f != 0.0,
        Value::String(s) => !s.is_empty() && s.to_lowercase() != "false" && s != "0",
        Value::Array(a) => !a.is_empty(),
        Value::Map(m) => !m.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_comparison_and_or() {
        let expr = parse_expression("github.event_name == 'push'");
        assert!(expr.is_ok(), "Failed to parse comparison: {:?}", expr.err());

        let expr = parse_expression(
            "github.event_name == 'push' || github.event_name == 'workflow_dispatch'",
        );
        assert!(
            expr.is_ok(),
            "Failed to parse complex expression: {:?}",
            expr.err()
        );
    }

    #[test]
    fn test_parse_simple_var() {
        let expr = parse_expression("true");
        assert!(expr.is_ok());
        assert!(matches!(
            expr.unwrap(),
            ExprNode::Literal(ExprValue::Bool(true))
        ));
    }

    #[test]
    fn test_parse_var_ref() {
        let expr = parse_expression("github.event_name");
        assert!(expr.is_ok(), "Failed: {:?}", expr.err());
        match expr.unwrap() {
            ExprNode::Var(v) => {
                assert_eq!(v.namespace, "github");
                assert_eq!(v.path, vec!["event_name"]);
            }
            other => panic!("Expected Var, got {:?}", other),
        }
    }

    fn eval_bool(source: &str, ctx: &Context) -> bool {
        let parsed = parse_expression(source).expect("expression should parse");
        evaluate_bool(&parsed, ctx).expect("expression should evaluate")
    }

    #[test]
    fn test_status_functions_follow_context_status() {
        let mut ctx = Context {
            status: crate::types::RunStatus::success(),
            ..Default::default()
        };

        assert!(eval_bool("success()", &ctx));
        assert!(!eval_bool("failure()", &ctx));
        assert!(eval_bool("always()", &ctx));
        assert!(!eval_bool("cancelled()", &ctx));

        ctx.status = crate::types::RunStatus::failure();
        assert!(!eval_bool("success()", &ctx));
        assert!(eval_bool("failure()", &ctx));
        assert!(eval_bool("always()", &ctx));

        // A skipped dependency is neither success nor failure; only
        // `always()` holds.
        ctx.status = crate::types::RunStatus::neutral();
        assert!(!eval_bool("success()", &ctx));
        assert!(!eval_bool("failure()", &ctx));
        assert!(eval_bool("always()", &ctx));
    }

    #[test]
    fn test_run_status_from_dependency_conclusions() {
        use crate::types::{RunStatus, StepConclusion};

        let all_good = RunStatus::from_conclusions(&[StepConclusion::Success]);
        assert!(all_good.success && !all_good.failure);

        let one_failed =
            RunStatus::from_conclusions(&[StepConclusion::Success, StepConclusion::Failure]);
        assert!(!one_failed.success && one_failed.failure);

        let one_skipped =
            RunStatus::from_conclusions(&[StepConclusion::Success, StepConclusion::Skipped]);
        assert!(!one_skipped.success && !one_skipped.failure);

        // No dependencies at all means nothing has failed.
        let empty = RunStatus::from_conclusions(&[]);
        assert!(empty.success);
    }

    #[test]
    fn test_needs_result_and_step_status() {
        use crate::types::{StepConclusion, StepStatus};

        let ctx = Context {
            job_results: [("build".to_string(), StepConclusion::Failure)]
                .into_iter()
                .collect(),
            step_status: [(
                "flaky".to_string(),
                StepStatus {
                    outcome: StepConclusion::Failure,
                    conclusion: StepConclusion::Success,
                },
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        };

        assert_eq!(
            evaluate_string("${{ needs.build.result }}", &ctx).unwrap(),
            "failure"
        );
        assert_eq!(
            evaluate_string("${{ steps.flaky.outcome }}", &ctx).unwrap(),
            "failure"
        );
        assert_eq!(
            evaluate_string("${{ steps.flaky.conclusion }}", &ctx).unwrap(),
            "success"
        );
        // An unknown job reads as empty rather than erroring.
        assert_eq!(
            evaluate_string("${{ needs.nope.result }}", &ctx).unwrap(),
            ""
        );
    }

    #[test]
    fn test_unknown_context_properties_render_empty() {
        let ctx = Context::default();
        assert_eq!(
            evaluate_string("${{ github.run_number }}", &ctx).unwrap(),
            ""
        );
        assert_eq!(evaluate_string("${{ runner.nonsense }}", &ctx).unwrap(), "");
    }

    #[test]
    fn test_mixed_type_comparisons_coerce_to_numbers() {
        let ctx = Context::default();

        // A string operand compared against a number is read numerically.
        assert!(eval_bool("'10' > 9", &ctx));
        assert!(!eval_bool("'10' < 9", &ctx));
        assert!(eval_bool("'10' >= 10", &ctx));
        assert!(eval_bool("1 == 1.0", &ctx));

        // Non-numeric operands fall back to string ordering rather than
        // silently answering false.
        assert!(eval_bool("'beta' > 'alpha'", &ctx));
        assert!(!eval_bool("'alpha' > 'beta'", &ctx));
    }

    /// The parser keeps its own list of namespaces. If it ever falls out of
    /// step with `evaluate_var`, expressions silently pass through as raw text
    /// — `${{ matrix.os }}` reaching the shell verbatim — so pin them together.
    #[test]
    fn test_every_known_namespace_parses_and_evaluates() {
        // One realistic reference per namespace.
        let probes = [
            ("github", "github.sha"),
            ("env", "env.FOO"),
            ("secrets", "secrets.TOKEN"),
            ("needs", "needs.build.result"),
            ("steps", "steps.build.outcome"),
            ("runner", "runner.os"),
            ("inputs", "inputs.version"),
            ("matrix", "matrix.os"),
            ("strategy", "strategy.job-index"),
            ("job", "job.status"),
            ("vars", "vars.REGISTRY"),
        ];

        // Adding a namespace without a probe here must fail, not slip through.
        let probed: Vec<&str> = probes.iter().map(|(ns, _)| *ns).collect();
        assert_eq!(
            probed, KNOWN_NAMESPACES,
            "every known namespace needs a probe in this test"
        );

        let ctx = Context::default();
        for (namespace, source) in probes {
            let parsed = parse_expression(source)
                .unwrap_or_else(|e| panic!("`{}` should parse: {}", source, e));

            match &parsed {
                ExprNode::Var(var) => assert_eq!(var.namespace, namespace),
                other => panic!("`{}` parsed as {:?}, expected a variable", source, other),
            }

            evaluate(&parsed, &ctx)
                .unwrap_or_else(|e| panic!("`{}` should evaluate: {}", source, e));
        }
    }

    #[test]
    fn a_comma_inside_a_string_does_not_split_arguments() {
        use std::collections::HashMap;

        let ctx = Context {
            env: HashMap::from([("LIST".to_string(), r#"["a","b"]"#.to_string())]),
            ..Default::default()
        };

        // `', '` is the idiomatic separator and its comma is not an argument
        // boundary.
        assert_eq!(
            evaluate_string("${{ join(fromJSON(env.LIST), ', ') }}", &ctx).unwrap(),
            "a, b"
        );
        assert_eq!(
            evaluate_string("${{ format('{0}, {1}', 'x', 'y') }}", &ctx).unwrap(),
            "x, y"
        );
        assert!(evaluate_bool(&parse_expression("contains('a,b', 'a,b')").unwrap(), &ctx).unwrap());
    }

    #[test]
    fn join_joins_the_elements_of_its_array() {
        use std::collections::HashMap;

        let ctx = Context {
            env: HashMap::from([("LIST".to_string(), r#"["a","b","c"]"#.to_string())]),
            ..Default::default()
        };

        // The default separator is a bare comma.
        assert_eq!(
            evaluate_string("${{ join(fromJSON(env.LIST)) }}", &ctx).unwrap(),
            "a,b,c"
        );
        // A single value is its own string rather than an error.
        assert_eq!(
            evaluate_string("${{ join('solo', '-') }}", &ctx).unwrap(),
            "solo"
        );
    }

    #[test]
    fn from_json_parses_and_its_result_can_be_reached_into() {
        use std::collections::HashMap;

        let ctx = Context {
            env: HashMap::from([(
                "SPEC".to_string(),
                r#"{"os":["linux","macos"],"deep":{"n":42},"flag":true}"#.to_string(),
            )]),
            ..Default::default()
        };

        // Property access on a function result is the whole point: this is the
        // shape a dynamic matrix arrives in.
        assert_eq!(
            evaluate_string("${{ fromJSON(env.SPEC).deep.n }}", &ctx).unwrap(),
            "42"
        );
        assert_eq!(
            evaluate_string("${{ fromJSON(env.SPEC).os[1] }}", &ctx).unwrap(),
            "macos"
        );
        // Types survive rather than everything becoming a string.
        assert!(
            evaluate_bool(&parse_expression("fromJSON(env.SPEC).flag").unwrap(), &ctx).unwrap()
        );
        assert_eq!(
            evaluate_string("${{ fromJSON('42') }}", &ctx).unwrap(),
            "42"
        );
        // A missing property reads as null, not an error.
        assert_eq!(
            evaluate_string("${{ fromJSON(env.SPEC).nope.deeper }}", &ctx).unwrap(),
            "null"
        );
    }

    #[test]
    fn from_json_rejects_something_that_is_not_json() {
        let ctx = Context::default();
        let error = evaluate_string("${{ fromJSON('not json') }}", &ctx).unwrap_err();
        assert!(error.to_string().contains("fromJSON"), "{}", error);
    }

    #[test]
    fn a_splat_maps_over_an_array() {
        use std::collections::HashMap;

        let ctx = Context {
            env: HashMap::from([(
                "COMMITS".to_string(),
                r#"[{"message":"one"},{"message":"two"}]"#.to_string(),
            )]),
            ..Default::default()
        };
        assert_eq!(
            evaluate_string("${{ join(fromJSON(env.COMMITS).*.message, ', ') }}", &ctx).unwrap(),
            "one, two"
        );
    }

    #[test]
    fn to_json_produces_json_and_round_trips() {
        use std::collections::HashMap;

        let ctx = Context {
            env: HashMap::from([("SPEC".to_string(), r#"{"b":2,"a":[1,"x"]}"#.to_string())]),
            ..Default::default()
        };

        // A string becomes a *quoted* string, which is what makes toJSON
        // usable for building JSON rather than just printing.
        assert_eq!(
            evaluate_string("${{ toJSON('hi') }}", &ctx).unwrap(),
            "\"hi\""
        );

        let rendered = evaluate_string("${{ toJSON(fromJSON(env.SPEC)) }}", &ctx).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(parsed["b"], 2);
        assert_eq!(parsed["a"][1], "x");
        // Keys come out sorted, so the same value renders the same way twice.
        assert!(rendered.find("\"a\"").unwrap() < rendered.find("\"b\"").unwrap());
    }

    #[test]
    fn hash_files_hashes_content_and_is_stable() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("Cargo.lock"), "lock v1").unwrap();
        std::fs::write(dir.path().join("src/a.txt"), "a").unwrap();

        let patterns = vec!["**/*.lock".to_string()];
        let first = hash_files(dir.path(), &patterns);
        assert_eq!(first.len(), 64, "expected a sha-256 hex digest: {}", first);
        // Same content, same key — this is what makes a cache hit possible.
        assert_eq!(first, hash_files(dir.path(), &patterns));

        // Different content, different key — this is what the old placeholder
        // got wrong, and it silently made every cache key identical.
        std::fs::write(dir.path().join("Cargo.lock"), "lock v2").unwrap();
        assert_ne!(first, hash_files(dir.path(), &patterns));
    }

    #[test]
    fn hash_files_covers_every_pattern_and_says_nothing_when_none_match() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.lock"), "a").unwrap();
        std::fs::write(dir.path().join("b.toml"), "b").unwrap();

        let both = hash_files(
            dir.path(),
            &["**/*.lock".to_string(), "**/*.toml".to_string()],
        );
        let one = hash_files(dir.path(), &["**/*.lock".to_string()]);
        assert_ne!(both, one);
        // Order of the patterns must not change the answer.
        assert_eq!(
            both,
            hash_files(
                dir.path(),
                &["**/*.toml".to_string(), "**/*.lock".to_string()]
            )
        );

        // No match is the empty string, so `hashFiles(...) != ''` is a usable
        // "did anything match" test.
        assert_eq!(hash_files(dir.path(), &["**/*.nope".to_string()]), "");
    }

    #[test]
    fn test_matrix_values_are_readable() {
        use std::collections::HashMap;

        let ctx = Context {
            matrix: HashMap::from([
                ("os".to_string(), Value::String("linux".to_string())),
                (
                    "target".to_string(),
                    Value::Map(HashMap::from([(
                        "format".to_string(),
                        Value::String("apk".to_string()),
                    )])),
                ),
            ]),
            ..Default::default()
        };

        assert_eq!(evaluate_string("${{ matrix.os }}", &ctx).unwrap(), "linux");
        assert_eq!(
            evaluate_string("${{ matrix.target.format }}", &ctx).unwrap(),
            "apk"
        );
        // Missing keys and missing sub-keys read as empty.
        assert_eq!(evaluate_string("${{ matrix.nope }}", &ctx).unwrap(), "");
        assert_eq!(
            evaluate_string("${{ matrix.target.nope }}", &ctx).unwrap(),
            ""
        );
    }

    #[test]
    fn test_strategy_context_is_readable() {
        let ctx = Context {
            strategy: crate::types::StrategyContext {
                fail_fast: false,
                job_index: 2,
                job_total: 5,
                max_parallel: None,
            },
            ..Default::default()
        };

        assert_eq!(
            evaluate_string("${{ strategy.job-index }}", &ctx).unwrap(),
            "2"
        );
        assert_eq!(
            evaluate_string("${{ strategy.job-total }}", &ctx).unwrap(),
            "5"
        );
        assert_eq!(
            evaluate_string("${{ strategy.fail-fast }}", &ctx).unwrap(),
            "false"
        );
    }

    #[test]
    fn test_short_ref_name() {
        assert_eq!(short_ref_name("refs/heads/main"), "main");
        assert_eq!(short_ref_name("refs/tags/v1.0.0"), "v1.0.0");
        assert_eq!(short_ref_name("main"), "main");
    }
}
