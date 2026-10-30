//! Parser-only native Rust diagnostic for a reviewed synthetic formula corpus.
//! Windows Rust corpus previously executed; portable-host validation is separate.
//! Dependencies: formualizer-parse = "=4.1.0", default-features=false;
//! formualizer-common = "=4.1.0", default-features=false (transitive pin);
//! serde_json = "=1.0.145". No evaluator, workbook engine or default facade.
//! Crate archive identifies commit dfd2f96457ff0929114a3e6b786c2761e6db5c5d.

use formualizer_parse::parser::{CollectPolicy, ParserError, RefView};
use formualizer_parse::{ASTNode, ASTNodeType, Parser, ParserLimits};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    panic::{AssertUnwindSafe, catch_unwind},
};

const COMMIT: &str = "dfd2f96457ff0929114a3e6b786c2761e6db5c5d";
const PARSER_SHA256: &str = "c3385b80d3b1be2244f62cf90a7dc8059409854e4220a61c69fcbbff3a25ec8e";
const MAX_REF_OUTPUT: usize = 128;
const MAX_FUNCTION_OUTPUT: usize = 64;
const MAX_RESULT_BYTES: usize = 96 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputSyntax {
    ExcelA1,
    ExcelR1C1,
}

pub fn diagnostic_limits() -> ParserLimits {
    ParserLimits::default()
        .with_source_bytes(4096)
        .with_tokens(2048)
        .with_ast_nodes(1024)
        .with_pratt_frames(32)
        .with_ast_height(64)
}

fn limits_json(limits: ParserLimits) -> Value {
    json!({
        "source_bytes": limits.source_bytes(), "tokens": limits.tokens(),
        "ast_nodes": limits.ast_nodes(), "pratt_frames": limits.pratt_frames(),
        "ast_height": limits.ast_height(),
    })
}

// The candidate exposes message + optional position, not a typed error category.
// These labels are diagnostic message-derived hints pinned to crate 4.1.0.
fn rejection(error: ParserError) -> Value {
    let message = error.message.to_ascii_lowercase();
    let category = if message.contains("source byte limit") {
        "source_budget"
    } else if message.contains("token limit") {
        "token_budget"
    } else if message.contains("ast node limit") {
        "ast_node_budget"
    } else if message.contains("ast height limit") {
        "ast_height_budget"
    } else if message.contains("nesting too deep") {
        "pratt_frame_budget"
    } else {
        // A parse error alone does not distinguish bad Excel syntax from an
        // otherwise valid Excel form absent from this candidate's grammar.
        "syntax_or_unsupported_unresolved"
    };
    json!({
        "status": "parser_rejected", "rejection_category": category,
        "category_is_message_derived": true,
        "message": error.message, "position": error.position,
    })
}

fn reference_json(reference: RefView<'_>) -> Value {
    match reference {
        RefView::Cell {
            sheet,
            row,
            col,
            row_abs,
            col_abs,
        } => json!({
            "kind":"cell", "sheet":sheet, "row":row, "column":col,
            "row_absolute":row_abs, "column_absolute":col_abs,
        }),
        RefView::Range {
            sheet,
            start_row,
            start_col,
            end_row,
            end_col,
            start_row_abs,
            start_col_abs,
            end_row_abs,
            end_col_abs,
        } => json!({
            "kind":"range", "sheet":sheet,
            "start_row":start_row, "start_column":start_col,
            "end_row":end_row, "end_column":end_col,
            "start_row_absolute":start_row_abs, "start_column_absolute":start_col_abs,
            "end_row_absolute":end_row_abs, "end_column_absolute":end_col_abs,
            "range_expanded":false,
        }),
        RefView::Cell3D {
            sheet_first,
            sheet_last,
            row,
            col,
            row_abs,
            col_abs,
        } => json!({
            "kind":"cell_3d", "first_sheet":sheet_first, "last_sheet":sheet_last,
            "row":row, "column":col, "row_absolute":row_abs, "column_absolute":col_abs,
            "sheet_span_resolved":false,
        }),
        RefView::Range3D {
            sheet_first,
            sheet_last,
            start_row,
            start_col,
            end_row,
            end_col,
            start_row_abs,
            start_col_abs,
            end_row_abs,
            end_col_abs,
        } => json!({
            "kind":"range_3d", "first_sheet":sheet_first, "last_sheet":sheet_last,
            "start_row":start_row, "start_column":start_col,
            "end_row":end_row, "end_column":end_col,
            "start_row_absolute":start_row_abs, "start_column_absolute":start_col_abs,
            "end_row_absolute":end_row_abs, "end_column_absolute":end_col_abs,
            "range_expanded":false, "sheet_span_resolved":false,
        }),
        RefView::External {
            raw,
            book,
            sheet,
            kind,
        } => json!({
            "kind":"external", "raw":raw, "book_token":book, "sheet":sheet,
            "reference_shape_debug":format!("{kind:?}"),
            "external_book_opened":false, "identity_resolved":false,
        }),
        RefView::Table { name, specifier } => json!({
            "kind":"table", "table_name":name,
            "specifier_debug":specifier.map(|value| format!("{value:?}")),
            "table_metadata_resolved":false,
        }),
        RefView::NamedRange { name } => json!({
            "kind":"name", "name":name, "scope_resolved":false,
            "may_be_lexical_variable_or_declaration":true,
        }),
    }
}

// Only recognized Excel compatibility prefixes are removed, for classification
// alone. Preserve the original AST and reported raw names; do not strip arbitrary
// namespaces such as Custom.LET or MyAddin.INDIRECT.
fn classification_name(mut name: &str) -> &str {
    while let Some((prefix, tail)) = name.split_once('.') {
        if prefix.eq_ignore_ascii_case("_xlfn") || prefix.eq_ignore_ascii_case("_xlws") {
            name = tail;
        } else {
            break;
        }
    }
    name
}

fn summarize(ast: &ASTNode, limits: ParserLimits) -> Value {
    let mut nodes = 0usize;
    let mut max_height = 0usize;
    let mut calls = 0usize;
    let mut functions = BTreeSet::new();
    let mut functions_truncated = false;
    let mut has_lexical_construct = false;
    let mut has_dynamic_function = false;
    let mut stack = vec![(ast, 1usize)];
    while let Some((node, height)) = stack.pop() {
        nodes = nodes.saturating_add(1);
        max_height = max_height.max(height);
        if nodes > limits.ast_nodes() || height > limits.ast_height() {
            return json!({"status":"host_invariant_failed", "reason":"AST exceeded configured budgets"});
        }
        match &node.node_type {
            ASTNodeType::UnaryOp { expr, .. } => stack.push((expr, height + 1)),
            ASTNodeType::BinaryOp { left, right, .. } => {
                stack.push((right, height + 1));
                stack.push((left, height + 1));
            }
            ASTNodeType::Function { name, args } => {
                // Classification must inspect every admitted AST node even when
                // the independently capped raw function-name output is full.
                let builtin = classification_name(name);
                has_lexical_construct |=
                    builtin.eq_ignore_ascii_case("LET") || builtin.eq_ignore_ascii_case("LAMBDA");
                has_dynamic_function |= builtin.eq_ignore_ascii_case("INDIRECT")
                    || builtin.eq_ignore_ascii_case("OFFSET");
                if functions.len() < MAX_FUNCTION_OUTPUT || functions.contains(name) {
                    functions.insert(name.clone());
                } else {
                    functions_truncated = true;
                }
                stack.extend(args.iter().rev().map(|child| (child, height + 1)));
            }
            ASTNodeType::Call { callee, args } => {
                calls = calls.saturating_add(1);
                stack.extend(args.iter().rev().map(|child| (child, height + 1)));
                stack.push((callee, height + 1));
            }
            ASTNodeType::Array(rows) => {
                for row in rows.iter().rev() {
                    stack.extend(row.iter().rev().map(|child| (child, height + 1)));
                }
            }
            ASTNodeType::Literal(_) | ASTNodeType::Omitted | ASTNodeType::Reference { .. } => {}
        }
    }
    let refs_total = ast.refs().count();
    let references: Vec<Value> = ast
        .refs()
        .take(MAX_REF_OUTPUT)
        .map(reference_json)
        .collect();
    let mut visit_count = 0usize;
    ast.visit_refs(|_| visit_count = visit_count.saturating_add(1));
    let dependency_count = ast.get_dependencies().len();
    // Both policies keep ranges symbolic. Only call the cloning convenience API
    // after proving the reference set fits our output cap.
    let (including_names, excluding_names) = if refs_total <= MAX_REF_OUTPUT {
        let include = CollectPolicy {
            expand_small_ranges: false,
            range_expansion_limit: 0,
            include_names: true,
        };
        let exclude = CollectPolicy {
            include_names: false,
            ..include.clone()
        };
        (
            Some(ast.collect_references(&include).len()),
            Some(ast.collect_references(&exclude).len()),
        )
    } else {
        (None, None)
    };
    json!({
        "status":"parsed_unresolved", "nodes":nodes, "ast_height":max_height,
        "function_names":functions, "function_names_truncated":functions_truncated,
        "call_expression_nodes":calls,
        "references":references, "references_total":refs_total,
        "references_truncated":refs_total > MAX_REF_OUTPUT,
        "visit_refs_count":visit_count, "get_dependencies_count":dependency_count,
        "reference_walk_counts_agree":refs_total == visit_count && refs_total == dependency_count,
        "collect_including_names_count":including_names,
        "collect_excluding_names_count":excluding_names,
        "lexical_binding_resolution_required":has_lexical_construct,
        "potential_dynamic_reference_function":has_dynamic_function,
        "ast_classification_scan_complete":true,
        "function_classification_scope":"known syntactic builtin candidates, not UDF or workbook-name resolution",
        "semantic_resolution_performed":false,
        "dependency_graph_complete":false,
        "volatility_classification":"not_supplied; contains_volatile is not a completeness result",
    })
}

/// Explicit syntax prevents R1C1 from silently entering an A1 parser as names.
/// `parsed_unresolved` is only syntactic acceptance, never a resolved graph.
#[cfg(test)]
pub fn inspect_formula(formula: &str, syntax: InputSyntax) -> Value {
    inspect_with_limits(formula, syntax, diagnostic_limits())
}

fn inspect_with_limits(formula: &str, syntax: InputSyntax, limits: ParserLimits) -> Value {
    if syntax == InputSyntax::ExcelR1C1 {
        return json!({
            "status":"host_unsupported_dialect", "parser_called":false,
            "reason":"candidate has Excel A1/OpenFormula dialects, no R1C1 mode",
            "next_step":"use a verified portable R1C1 parser/normalizer at a known origin; native Excel is an optional compatibility oracle",
        });
    }
    match catch_unwind(AssertUnwindSafe(|| {
        let output = match Parser::builder().limits(limits).parse(formula) {
            Ok(ast) => summarize(&ast, limits), // bounded AST drops inside containment
            Err(error) => rejection(error),
        };
        match serde_json::to_vec(&output) {
            Ok(bytes) if bytes.len() <= MAX_RESULT_BYTES => output,
            _ => json!({"status":"host_output_budget", "max_result_bytes":MAX_RESULT_BYTES}),
        }
    })) {
        Ok(output) => output,
        Err(payload) => {
            // Prevent a custom payload destructor from causing a second panic.
            std::mem::forget(payload);
            json!({"status":"caught_panic", "worker_should_be_recycled":true})
        }
    }
}

enum Expected {
    Parsed(&'static [&'static str]),
    Rejected(&'static str),
    UnsupportedDialect,
}

struct Case {
    id: &'static str,
    formula: String,
    syntax: InputSyntax,
    limits: ParserLimits,
    expected: Expected,
}

type ParsedCase = (&'static str, &'static str, &'static [&'static str]);

fn cases() -> Vec<Case> {
    let parsed: &[ParsedCase] = &[
        ("mixed_anchors", "=$A1+B$2+$C$3", &["cell"]),
        ("quoted_sheet", "='Inputs Data'!B2+Sheet2!A1", &["cell"]),
        ("whole_column_symbolic", "=SUM(A:A)", &["range"]),
        ("whole_row_symbolic", "=SUM(2:4)", &["range"]),
        (
            "three_dimensional",
            "=SUM(Sheet1:Sheet3!A1:B2)",
            &["range_3d"],
        ),
        (
            "external_book",
            "='C:\\models\\[Book.xlsx]Inputs'!$A$1",
            &["external"],
        ),
        ("table_column", "=SUM(Sales[Amount])", &["table"]),
        ("table_current_row", "=[@Amount]-[@Cost]", &["table"]),
        ("workbook_name", "=PriceInput*2", &["name"]),
        (
            "let_raw_lexical_names",
            "=LET(x,A1,x+B1)",
            &["name", "cell"],
        ),
        (
            "immediate_lambda_syntax",
            "=LAMBDA(x,x+1)(A1)",
            &["name", "cell"],
        ),
        ("raw_a1_r1c1_looking_absolute", "=R1C1", &["name"]),
        ("raw_a1_r1c1_looking_relative", "=R[1]C[-2]", &["name"]),
        (
            "dynamic_string_no_cell_edge",
            "=INDIRECT(\"Inputs!A2\")",
            &[],
        ),
        (
            "dynamic_offset_base_only",
            "=OFFSET(Inputs!A1,1,0)",
            &["cell"],
        ),
        (
            "unknown_function_is_only_syntax",
            "=CustomAddinFn(A1)",
            &["cell"],
        ),
        ("spill", "=A1#", &["cell"]),
        ("implicit_intersection", "=@A1:A3", &["range"]),
        ("reference_union", "=SUM((A1:A2,C1:C2))", &["range"]),
        ("reference_intersection", "=SUM(A1:C3 B2:D4)", &["range"]),
        ("omitted_argument", "=SUM(A1,,B1)", &["cell"]),
        ("array", "={1,2;3,4}", &[]),
    ];
    let mut out: Vec<Case> = parsed
        .iter()
        .copied()
        .map(|(id, formula, kinds)| Case {
            id,
            formula: formula.to_owned(),
            syntax: InputSyntax::ExcelA1,
            limits: diagnostic_limits(),
            expected: Expected::Parsed(kinds),
        })
        .collect();
    out.push(Case {
        id: "r1c1_contract_refusal",
        formula: "=R[1]C[-2]".to_owned(),
        syntax: InputSyntax::ExcelR1C1,
        limits: diagnostic_limits(),
        expected: Expected::UnsupportedDialect,
    });
    out.push(Case {
        id: "malformed_unclosed_call",
        formula: "=SUM(".to_owned(),
        syntax: InputSyntax::ExcelA1,
        limits: diagnostic_limits(),
        expected: Expected::Rejected("syntax_or_unsupported_unresolved"),
    });
    // Small exact boundary values are source-backed by upstream resource_limits.rs.
    let tiny = ParserLimits::default()
        .with_source_bytes(4)
        .with_tokens(3)
        .with_ast_nodes(3)
        .with_pratt_frames(2)
        .with_ast_height(2);
    out.push(Case {
        id: "tiny_all_budgets_exact",
        formula: "=1+1".to_owned(),
        syntax: InputSyntax::ExcelA1,
        limits: tiny,
        expected: Expected::Parsed(&[]),
    });
    for (id, limits, category) in [
        (
            "source_over_budget",
            tiny.with_source_bytes(3),
            "source_budget",
        ),
        ("tokens_over_budget", tiny.with_tokens(2), "token_budget"),
        (
            "nodes_over_budget",
            tiny.with_ast_nodes(2),
            "ast_node_budget",
        ),
        (
            "frames_over_budget",
            tiny.with_pratt_frames(1),
            "pratt_frame_budget",
        ),
        (
            "height_over_budget",
            tiny.with_ast_height(1),
            "ast_height_budget",
        ),
    ] {
        out.push(Case {
            id,
            formula: "=1+1".to_owned(),
            syntax: InputSyntax::ExcelA1,
            limits,
            expected: Expected::Rejected(category),
        });
    }
    out
}

fn expectation_holds(observed: &Value, expected: &Expected) -> bool {
    let status = observed.get("status").and_then(Value::as_str);
    match expected {
        Expected::UnsupportedDialect => status == Some("host_unsupported_dialect"),
        Expected::Rejected(category) => {
            status == Some("parser_rejected")
                && observed.get("rejection_category").and_then(Value::as_str) == Some(*category)
        }
        Expected::Parsed(kinds) => {
            status == Some("parsed_unresolved")
                && observed
                    .get("reference_walk_counts_agree")
                    .and_then(Value::as_bool)
                    == Some(true)
                && kinds.iter().all(|kind| {
                    observed
                        .get("references")
                        .and_then(Value::as_array)
                        .is_some_and(|refs| {
                            refs.iter().any(|reference| {
                                reference.get("kind").and_then(Value::as_str) == Some(*kind)
                            })
                        })
                })
        }
    }
}

/// Returns only reviewed synthetic observations, with separate expected/observed
/// checks. The identity constants are archive expectations; verify Cargo.lock
/// checksums and cargo tree rather than treating them as runtime introspection.
pub fn run_parser_candidate_probe() -> Value {
    let rows: Vec<Value> = cases().into_iter().map(|case| {
        let observed = inspect_with_limits(&case.formula, case.syntax, case.limits);
        let passed = expectation_holds(&observed, &case.expected);
        json!({"id":case.id, "passed":passed, "limits":limits_json(case.limits), "observed":observed})
    }).collect();
    let passed = rows
        .iter()
        .all(|row| row.get("passed").and_then(Value::as_bool) == Some(true));
    json!({
        "passed":passed, "component":"parser_only", "expected_crate":"formualizer-parse 4.1.0",
        "archive_vcs_commit":COMMIT, "expected_archive_sha256":PARSER_SHA256,
        "evaluator_used":false, "excel_oracle_used":false, "parser_selected":false,
        "diagnostic_limits":limits_json(diagnostic_limits()), "cases":rows,
        "limitations":[
            "syntactic acceptance is not function support or lexical/workbook resolution",
            "raw R1C1-shaped names are not R1C1 reference semantics",
            "dynamic strings are not resolved dependency edges",
            "parsed names can be LET/LAMBDA declarations or bound variables",
            "parse rejection alone does not separate malformed input from missing grammar",
            "catch_unwind cannot catch abort, OOM, access violation or bound wall time/RSS",
            "AST drop/display/clone are not all iterative; limits must not be raised casually"
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_native_parser_corpus_matches_explicit_expectations() {
        let report = run_parser_candidate_probe();
        assert_eq!(
            report.get("passed").and_then(Value::as_bool),
            Some(true),
            "{report:#}"
        );
    }

    #[test]
    fn declared_r1c1_is_refused_while_raw_a1_lookalikes_remain_unresolved_names() {
        for formula in ["=R1C1", "=R[1]C[-2]"] {
            let refused = inspect_formula(formula, InputSyntax::ExcelR1C1);
            assert_eq!(
                refused.get("status").and_then(Value::as_str),
                Some("host_unsupported_dialect")
            );
            let raw = inspect_formula(formula, InputSyntax::ExcelA1);
            assert!(expectation_holds(&raw, &Expected::Parsed(&["name"])));
            assert_eq!(
                raw.get("semantic_resolution_performed")
                    .and_then(Value::as_bool),
                Some(false)
            );
        }
    }

    #[test]
    fn lexical_declarations_and_uses_are_visible_as_raw_names() {
        for formula in ["=LET(x,A1,x+B1)", "=LAMBDA(x,x+1)(A1)"] {
            let value = inspect_formula(formula, InputSyntax::ExcelA1);
            assert_eq!(
                value
                    .get("lexical_binding_resolution_required")
                    .and_then(Value::as_bool),
                Some(true)
            );
            let include = value
                .get("collect_including_names_count")
                .and_then(Value::as_u64)
                .expect("count");
            let exclude = value
                .get("collect_excluding_names_count")
                .and_then(Value::as_u64)
                .expect("count");
            assert!(
                include >= exclude + 2,
                "declaration and use are not resolved edges: {value}"
            );
        }
    }

    #[test]
    fn entire_column_stays_one_symbolic_range_and_indirect_text_has_no_cell_edge() {
        let column = inspect_formula("=SUM(A:A)", InputSyntax::ExcelA1);
        assert_eq!(
            column.get("references_total").and_then(Value::as_u64),
            Some(1)
        );
        let indirect = inspect_formula("=INDIRECT(\"Inputs!A2\")", InputSyntax::ExcelA1);
        assert_eq!(
            indirect.get("references_total").and_then(Value::as_u64),
            Some(0)
        );
        assert_eq!(
            indirect
                .get("potential_dynamic_reference_function")
                .and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            indirect
                .get("dependency_graph_complete")
                .and_then(Value::as_bool),
            Some(false)
        );
    }

    #[test]
    fn broad_reference_output_is_explicitly_truncated_without_range_expansion() {
        let formula = format!("=SUM({})", vec!["A1"; MAX_REF_OUTPUT + 1].join(","));
        let value = inspect_formula(&formula, InputSyntax::ExcelA1);
        assert_eq!(
            value.get("status").and_then(Value::as_str),
            Some("parsed_unresolved")
        );
        assert_eq!(
            value.get("references_total").and_then(Value::as_u64),
            Some((MAX_REF_OUTPUT + 1) as u64)
        );
        assert_eq!(
            value.get("references_truncated").and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            value
                .get("references")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(MAX_REF_OUTPUT)
        );
        assert_eq!(
            value.get("collect_including_names_count"),
            Some(&Value::Null)
        );
    }

    #[test]
    fn source_bytes_are_utf8_and_zero_budgets_are_not_silently_raised() {
        let one_node = ParserLimits::default()
            .with_source_bytes(3)
            .with_tokens(1)
            .with_ast_nodes(1)
            .with_pratt_frames(1)
            .with_ast_height(1);
        let at = inspect_with_limits("=\u{00e9}", InputSyntax::ExcelA1, one_node);
        assert!(expectation_holds(&at, &Expected::Parsed(&["name"])));
        let above = inspect_with_limits(
            "=\u{00e9}",
            InputSyntax::ExcelA1,
            one_node.with_source_bytes(2),
        );
        assert!(expectation_holds(
            &above,
            &Expected::Rejected("source_budget")
        ));
        let zero = inspect_with_limits(
            "=1",
            InputSyntax::ExcelA1,
            diagnostic_limits().with_ast_nodes(0),
        );
        assert!(expectation_holds(
            &zero,
            &Expected::Rejected("ast_node_budget")
        ));
    }

    #[test]
    fn truncated_function_names_do_not_truncate_classification() {
        let filler = (0..MAX_FUNCTION_OUTPUT + 1)
            .map(|index| format!("CandidateFn{index}()"))
            .collect::<Vec<_>>()
            .join(",");
        for (tail, lexical, dynamic) in [
            ("INDIRECT(\"A1\")", false, true),
            ("OFFSET(A1,1,0)", false, true),
            ("LET(x,A1,x)", true, false),
            ("LAMBDA(x,x)(A1)", true, false),
        ] {
            let formula = format!("=SUM({filler},{tail})");
            let value = inspect_formula(&formula, InputSyntax::ExcelA1);
            assert_eq!(
                value.get("status").and_then(Value::as_str),
                Some("parsed_unresolved"),
                "{value}"
            );
            assert_eq!(
                value
                    .get("function_names_truncated")
                    .and_then(Value::as_bool),
                Some(true)
            );
            assert_eq!(
                value
                    .get("function_names")
                    .and_then(Value::as_array)
                    .map(Vec::len),
                Some(MAX_FUNCTION_OUTPUT)
            );
            assert_eq!(
                value
                    .get("lexical_binding_resolution_required")
                    .and_then(Value::as_bool),
                Some(lexical),
                "{value}"
            );
            assert_eq!(
                value
                    .get("potential_dynamic_reference_function")
                    .and_then(Value::as_bool),
                Some(dynamic),
                "{value}"
            );
            assert_eq!(
                value
                    .get("ast_classification_scan_complete")
                    .and_then(Value::as_bool),
                Some(true)
            );
        }
    }

    #[test]
    fn compatibility_prefixes_classify_without_rewriting_raw_names() {
        for (formula, raw_name, lexical, dynamic) in [
            ("=_xlfn.LET(x,A1,x)", "_xlfn.LET", true, false),
            ("=_xlfn.LAMBDA(x,x)(A1)", "_xlfn.LAMBDA", true, false),
            ("=_xlws.INDIRECT(\"A1\")", "_xlws.INDIRECT", false, true),
            (
                "=_xlfn._xlws.OFFSET(A1,1,0)",
                "_xlfn._xlws.OFFSET",
                false,
                true,
            ),
            ("=_XLFN._XLWS.LET(x,A1,x)", "_XLFN._XLWS.LET", true, false),
            ("=Custom.LET(A1)", "Custom.LET", false, false),
            ("=MyAddin.INDIRECT(A1)", "MyAddin.INDIRECT", false, false),
        ] {
            let value = inspect_formula(formula, InputSyntax::ExcelA1);
            assert_eq!(
                value.get("status").and_then(Value::as_str),
                Some("parsed_unresolved"),
                "{value}"
            );
            let names = value
                .get("function_names")
                .and_then(Value::as_array)
                .expect("raw names");
            assert!(
                names.iter().any(|name| name.as_str() == Some(raw_name)),
                "{value}"
            );
            assert_eq!(
                value
                    .get("lexical_binding_resolution_required")
                    .and_then(Value::as_bool),
                Some(lexical),
                "{value}"
            );
            assert_eq!(
                value
                    .get("potential_dynamic_reference_function")
                    .and_then(Value::as_bool),
                Some(dynamic),
                "{value}"
            );
        }
    }

    #[test]
    fn bare_unicode_text_is_a_literal_not_a_named_formula_reference() {
        let value = inspect_formula("\u{00e9}", InputSyntax::ExcelA1);
        assert_eq!(
            value.get("status").and_then(Value::as_str),
            Some("parsed_unresolved")
        );
        assert_eq!(
            value.get("references_total").and_then(Value::as_u64),
            Some(0)
        );
    }
}
