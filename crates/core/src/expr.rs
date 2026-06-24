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

use crate::types::{Context, Value};
use crate::types::WorkflowError;

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
    Unary {
        op: UnaryOp,
        expr: Box<ExprNode>,
    },
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

// ---------------------------------------------------------------------------
// Recursive descent expression parser
// ---------------------------------------------------------------------------

struct Parser<'a> {
    input: &'a str,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str) -> Self {
        Self { input: input.trim(), pos: 0 }
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
    fn parse_primary(&mut self) -> Result<ExprNode, ExprError> {
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
                    None => return Err(ExprError::InvalidSyntax(
                        "Unclosed string literal".to_string()
                    )),
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
            return Err(ExprError::InvalidSyntax(
                "Expected expression".to_string(),
            ));
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
            match namespace {
                "github" | "env" | "secrets" | "needs" | "steps" | "runner" | "inputs" => {
                    let path: Vec<String> = ident[dot + 1..]
                        .split('.')
                        .map(|s| s.to_string())
                        .collect();
                    return Ok(ExprNode::Var(VarRef {
                        namespace: namespace.to_string(),
                        path,
                    }));
                }
                _ => {}
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
    Err(ExprError::InvalidSyntax(format!("Not a literal: {}", input)))
}

/// Split function arguments by comma, respecting nested parentheses.
fn split_args(input: &str) -> Vec<&str> {
    let mut args = Vec::new();
    let mut depth = 0;
    let mut start = 0;

    for (i, c) in input.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
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
        ExprNode::Ternary { condition, then_branch, else_branch } => {
            let cond_val = evaluate(condition, ctx)?;
            let truthy = is_truthy(&cond_val);
            if truthy {
                evaluate(then_branch, ctx)
            } else {
                evaluate(else_branch, ctx)
            }
        }
        ExprNode::Pipeline { left, right } => {
            let left_val = evaluate(left, ctx)?;
            // Right side should be a function that takes the left value
            if let ExprNode::Func(func) = right.as_ref() {
                evaluate_func_with_arg(func, &left_val, ctx)
            } else {
                Err(WorkflowError::ExpressionError(
                    "Pipeline right side must be a function".to_string()
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
        _ => Err(WorkflowError::ExpressionError(
            format!("Unknown variable namespace: {}", var.namespace)
        )),
    }
}

fn evaluate_github_var(path: &[String], ctx: &Context) -> Result<Value, WorkflowError> {
    if path.is_empty() {
        return Ok(Value::String(ctx.github.event_name.clone()));
    }
    match path[0].as_str() {
        "event_name" => Ok(Value::String(ctx.github.event_name.clone())),
        "event" => Ok(Value::String(serde_json::to_string(&ctx.github.event).unwrap_or_default())),
        "repository" => Ok(Value::String(ctx.github.repository.clone())),
        "ref" => Ok(Value::String(ctx.github.ref_name.clone())),
        "sha" => Ok(Value::String(ctx.github.sha.clone())),
        "workspace" => Ok(Value::String(ctx.github.workspace.clone())),
        "action" => Ok(Value::String(ctx.github.action.clone())),
        "actor" => Ok(Value::String(ctx.github.actor.clone())),
        other => Err(WorkflowError::ExpressionError(
            format!("Unknown github variable: {}", other)
        )),
    }
}

fn evaluate_env_var(path: &[String], ctx: &Context) -> Result<Value, WorkflowError> {
    if path.is_empty() {
        return Ok(Value::Map(ctx.env.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect()));
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
            "needs variable requires at least job_id and output_name".to_string()
        ));
    }
    let job_id = &path[0];
    let output_name = &path[1];
    let output_name = if output_name == "outputs" && path.len() > 2 {
        &path[2]
    } else {
        output_name
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
    if path.len() < 3 {
        return Err(WorkflowError::ExpressionError(
            "steps variable requires step_id.outputs.key format".to_string()
        ));
    }
    let step_id = &path[0];
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
        other => Err(WorkflowError::ExpressionError(
            format!("Unknown runner variable: {}", other)
        )),
    }
}

fn evaluate_inputs_var(path: &[String], ctx: &Context) -> Result<Value, WorkflowError> {
    if path.is_empty() {
        return Ok(Value::Map(ctx.inputs.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect()));
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

fn evaluate_func_with_arg(func: &FuncCall, piped_arg: &Value, ctx: &Context) -> Result<Value, WorkflowError> {
    let builtin = BuiltinFunction::from_name(&func.name)
        .ok_or_else(|| WorkflowError::ExpressionError(
            format!("Unknown function: {}", func.name)
        ))?;

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
                return Err(WorkflowError::ExpressionError("contains requires at least 2 arguments".to_string()));
            }
            let haystack = args[0].to_string();
            let needle = args[1].to_string();
            Ok(Value::Bool(haystack.contains(&needle)))
        }
        BuiltinFunction::StartsWith => {
            if args.len() < 2 {
                return Err(WorkflowError::ExpressionError("startsWith requires 2 arguments".to_string()));
            }
            let s = args[0].to_string();
            let prefix = args[1].to_string();
            Ok(Value::Bool(s.starts_with(&prefix)))
        }
        BuiltinFunction::EndsWith => {
            if args.len() < 2 {
                return Err(WorkflowError::ExpressionError("endsWith requires 2 arguments".to_string()));
            }
            let s = args[0].to_string();
            let suffix = args[1].to_string();
            Ok(Value::Bool(s.ends_with(&suffix)))
        }
        BuiltinFunction::Format => {
            if args.is_empty() {
                return Err(WorkflowError::ExpressionError("format requires at least 1 argument".to_string()));
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
                return Err(WorkflowError::ExpressionError("join requires arguments".to_string()));
            }
            let separator = if args.len() > 1 { args[args.len() - 1].to_string() } else { ",".to_string() };
            let values: Vec<String> = args[..args.len() - 1].iter().map(|v| v.to_string()).collect();
            Ok(Value::String(values.join(&separator)))
        }
        BuiltinFunction::ToJSON => {
            if args.is_empty() {
                return Err(WorkflowError::ExpressionError("toJSON requires 1 argument".to_string()));
            }
            Ok(Value::String(args[0].to_string()))
        }
        BuiltinFunction::FromJSON => {
            if args.is_empty() {
                return Err(WorkflowError::ExpressionError("fromJSON requires 1 argument".to_string()));
            }
            Ok(Value::String(args[0].to_string()))
        }
        BuiltinFunction::HashFiles => {
            // Simplified: return a hash of the workspace
            Ok(Value::String("hash_placeholder".to_string()))
        }
        BuiltinFunction::Success => {
            Ok(Value::Bool(true)) // In local execution, assume success unless failed
        }
        BuiltinFunction::Failure => {
            Ok(Value::Bool(false))
        }
        BuiltinFunction::Always => {
            Ok(Value::Bool(true))
        }
        BuiltinFunction::Cancelled => {
            Ok(Value::Bool(false))
        }
    }
}

fn evaluate_comparison(op: &ComparisonOp, left: &Value, right: &Value) -> Result<Value, WorkflowError> {
    let result = match (left, right) {
        (Value::String(l), Value::String(r)) => {
            match op {
                ComparisonOp::Eq => l == r,
                ComparisonOp::Ne => l != r,
                ComparisonOp::Lt => l < r,
                ComparisonOp::Gt => l > r,
                ComparisonOp::Le => l <= r,
                ComparisonOp::Ge => l >= r,
            }
        }
        (Value::Int(l), Value::Int(r)) => {
            match op {
                ComparisonOp::Eq => l == r,
                ComparisonOp::Ne => l != r,
                ComparisonOp::Lt => l < r,
                ComparisonOp::Gt => l > r,
                ComparisonOp::Le => l <= r,
                ComparisonOp::Ge => l >= r,
            }
        }
        (Value::Float(l), Value::Float(r)) => {
            match op {
                ComparisonOp::Eq => (l - r).abs() < f64::EPSILON,
                ComparisonOp::Ne => (l - r).abs() >= f64::EPSILON,
                ComparisonOp::Lt => l < r,
                ComparisonOp::Gt => l > r,
                ComparisonOp::Le => l <= r,
                ComparisonOp::Ge => l >= r,
            }
        }
        (Value::Bool(l), Value::Bool(r)) => {
            match op {
                ComparisonOp::Eq => l == r,
                ComparisonOp::Ne => l != r,
                _ => false,
            }
        }
        _ => {
            // Fall back to string comparison
            let l = left.to_string();
            let r = right.to_string();
            match op {
                ComparisonOp::Eq => l == r,
                ComparisonOp::Ne => l != r,
                _ => l.cmp(&r) == std::cmp::Ordering::Equal && false,
            }
        }
    };
    Ok(Value::Bool(result))
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

        let expr = parse_expression("github.event_name == 'push' || github.event_name == 'workflow_dispatch'");
        assert!(expr.is_ok(), "Failed to parse complex expression: {:?}", expr.err());
    }

    #[test]
    fn test_parse_simple_var() {
        let expr = parse_expression("true");
        assert!(expr.is_ok());
        assert!(matches!(expr.unwrap(), ExprNode::Literal(ExprValue::Bool(true))));
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
}
