//! Extracts module requests from one source file with the Oxc parser.
//!
//! Collected as dependencies (spec section 7):
//! - `import ... from "x"`, `import "x"`, `import type ...`;
//! - `export ... from "x"`, `export * from "x"`;
//! - `import("x")`, `require("x")`, `require.resolve("x")`,
//!   `import.meta.resolve("x")` with a literal argument;
//! - `import x = require("x")` and `import("x")` types;
//! - literal module mocks (`vi.mock("x")`, `jest.mock("x")`,
//!   `mock.module("x")`): the test depends on the shape of the mocked module.
//!
//! Reported as uncertainty, never dropped:
//! - a non-literal argument of the calls above;
//! - `import.meta.glob` and `require.context`;
//! - a file that does not parse.
//!
//! Parsing establishes syntax only. It says nothing about files a test reads
//! at run time; those belong in `inputs` / `global_inputs`.

use std::collections::BTreeSet;
use std::path::Path;

use oxc_allocator::Allocator;
use oxc_ast::ast::{
    Argument, CallExpression, ExportAllDeclaration, ExportFromDeclaration, Expression,
    ImportDeclaration, ImportExpression, StringLiteral, TSExternalModuleReference, TSImportType,
};
use oxc_ast_visit::{Visit, walk};
use oxc_parser::Parser;
use oxc_span::SourceType;
use testry_domain::graph::GraphDiagnosticKind;
use testry_domain::paths::RepoPath;

/// What one file asks for, independent of how specifiers resolve.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ModuleRequests {
    /// Literal specifiers, exactly as written.
    pub(crate) specifiers: BTreeSet<String>,
    /// What could not be established for this file.
    pub(crate) problems: BTreeSet<(GraphDiagnosticKind, String)>,
}

impl ModuleRequests {
    pub(crate) fn problem(kind: GraphDiagnosticKind, detail: &str) -> Self {
        Self {
            specifiers: BTreeSet::new(),
            problems: BTreeSet::from([(kind, detail.to_owned())]),
        }
    }
}

pub(crate) fn extract(path: &RepoPath, bytes: &[u8]) -> ModuleRequests {
    let Ok(source) = std::str::from_utf8(bytes) else {
        return ModuleRequests::problem(
            GraphDiagnosticKind::ParseFailure,
            "the file is not valid UTF-8",
        );
    };
    let Ok(source_type) = SourceType::from_path(Path::new(path.as_str())) else {
        return ModuleRequests::problem(
            GraphDiagnosticKind::ParseFailure,
            "the file extension is not a supported source type",
        );
    };
    if let Some(requests) = parse(source, source_type) {
        return requests;
    }
    // `.js` files may contain JSX (Bun accepts it); try once more with JSX
    // enabled before giving up.
    let plain_javascript = matches!(path.extension(), Some("js" | "mjs" | "cjs"));
    if plain_javascript && let Some(requests) = parse(source, source_type.with_jsx(true)) {
        return requests;
    }
    ModuleRequests::problem(GraphDiagnosticKind::ParseFailure, "syntax error")
}

/// `None` when the file has syntax errors. A partially recovered tree is not
/// used: imports could be missing from it.
fn parse(source: &str, source_type: SourceType) -> Option<ModuleRequests> {
    let allocator = Allocator::new();
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if parsed.fatal_error || parsed.diagnostics.has_errors() {
        return None;
    }
    let mut collector = Collector {
        requests: ModuleRequests::default(),
    };
    collector.visit_program(&parsed.program);
    Some(collector.requests)
}

enum StaticString<'a> {
    Literal(&'a str),
    /// A string literal that is not valid Unicode (a lone surrogate).
    Unrepresentable,
    Dynamic,
}

fn static_string<'a>(expression: &Expression<'a>) -> StaticString<'a> {
    match expression.get_inner_expression() {
        Expression::StringLiteral(literal) => literal
            .value
            .as_str()
            .map_or(StaticString::Unrepresentable, StaticString::Literal),
        Expression::TemplateLiteral(template) if template.is_no_substitution_template() => {
            match template.single_quasi().and_then(|quasi| quasi.as_str()) {
                Some(text) => StaticString::Literal(text),
                None => StaticString::Unrepresentable,
            }
        }
        _ => StaticString::Dynamic,
    }
}

const MOCK_FUNCTIONS: [&str; 8] = [
    "mock",
    "doMock",
    "unmock",
    "doUnmock",
    "importActual",
    "importMock",
    "requireActual",
    "requireMock",
];

struct Collector {
    requests: ModuleRequests,
}

impl Collector {
    fn problem(&mut self, kind: GraphDiagnosticKind, detail: &str) {
        self.requests.problems.insert((kind, detail.to_owned()));
    }

    fn literal(&mut self, literal: &StringLiteral<'_>) {
        match literal.value.as_str() {
            Some(specifier) => {
                self.requests.specifiers.insert(specifier.to_owned());
            }
            None => self.unrepresentable(),
        }
    }

    fn unrepresentable(&mut self) {
        self.problem(
            GraphDiagnosticKind::UnsupportedMechanism,
            "a module specifier is not valid Unicode",
        );
    }

    /// A specifier position that holds an arbitrary expression.
    fn expression(&mut self, expression: &Expression<'_>, dynamic_detail: &str) {
        match static_string(expression) {
            StaticString::Literal(specifier) => {
                self.requests.specifiers.insert(specifier.to_owned());
            }
            StaticString::Unrepresentable => self.unrepresentable(),
            StaticString::Dynamic => {
                self.problem(GraphDiagnosticKind::DynamicDependency, dynamic_detail);
            }
        }
    }

    fn first_argument(&mut self, call: &CallExpression<'_>, dynamic_detail: &str) {
        match call.arguments.first().map(Argument::as_expression) {
            // No argument: nothing is requested.
            None => {}
            // A spread element.
            Some(None) => self.problem(GraphDiagnosticKind::DynamicDependency, dynamic_detail),
            Some(Some(expression)) => self.expression(expression, dynamic_detail),
        }
    }

    fn call(&mut self, call: &CallExpression<'_>) {
        let callee = call.callee.get_inner_expression();
        if callee.is_specific_id("require") {
            self.first_argument(call, "require(<expression>)");
            return;
        }
        let Expression::StaticMemberExpression(member) = callee else {
            return;
        };
        let property = member.property.name.as_str();
        let object = member.object.get_inner_expression();
        if object.is_specific_id("require") {
            match property {
                "resolve" => self.first_argument(call, "require.resolve(<expression>)"),
                "context" => {
                    self.problem(GraphDiagnosticKind::UnsupportedMechanism, "require.context");
                }
                _ => {}
            }
        } else if matches!(object, Expression::ImportMeta(_)) {
            match property {
                "glob" | "globEager" => {
                    self.problem(
                        GraphDiagnosticKind::UnsupportedMechanism,
                        "import.meta.glob",
                    );
                }
                "resolve" => self.first_argument(call, "import.meta.resolve(<expression>)"),
                _ => {}
            }
        } else if is_module_mock(object, property) {
            // `vi.mock(import("./x"))` is covered by the import expression.
            let wraps_import = call
                .arguments
                .first()
                .and_then(Argument::as_expression)
                .is_some_and(|argument| {
                    matches!(
                        argument.get_inner_expression(),
                        Expression::ImportExpression(_)
                    )
                });
            if !wraps_import {
                self.first_argument(call, "module mock with a non-literal module");
            }
        }
    }
}

fn is_module_mock(object: &Expression<'_>, property: &str) -> bool {
    let test_api = object.is_specific_id("vi")
        || object.is_specific_id("jest")
        || object.is_specific_id("vitest");
    (test_api && MOCK_FUNCTIONS.contains(&property))
        || (object.is_specific_id("mock") && property == "module")
}

impl<'a> Visit<'a> for Collector {
    fn visit_import_declaration(&mut self, it: &ImportDeclaration<'a>) {
        self.literal(&it.source);
        walk::walk_import_declaration(self, it);
    }

    fn visit_export_from_declaration(&mut self, it: &ExportFromDeclaration<'a>) {
        self.literal(&it.source);
        walk::walk_export_from_declaration(self, it);
    }

    fn visit_export_all_declaration(&mut self, it: &ExportAllDeclaration<'a>) {
        self.literal(&it.source);
        walk::walk_export_all_declaration(self, it);
    }

    fn visit_import_expression(&mut self, it: &ImportExpression<'a>) {
        self.expression(&it.source, "import(<expression>)");
        walk::walk_import_expression(self, it);
    }

    fn visit_call_expression(&mut self, it: &CallExpression<'a>) {
        self.call(it);
        walk::walk_call_expression(self, it);
    }

    fn visit_ts_import_type(&mut self, it: &TSImportType<'a>) {
        self.literal(&it.source);
        walk::walk_ts_import_type(self, it);
    }

    fn visit_ts_external_module_reference(&mut self, it: &TSExternalModuleReference<'a>) {
        self.literal(&it.expression);
        walk::walk_ts_external_module_reference(self, it);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn requests(name: &str, source: &str) -> ModuleRequests {
        extract(&RepoPath::try_new(name).unwrap(), source.as_bytes())
    }

    fn specifiers(name: &str, source: &str) -> Vec<String> {
        let requests = requests(name, source);
        assert!(
            requests.problems.is_empty(),
            "unexpected problems: {:?}",
            requests.problems
        );
        requests.specifiers.into_iter().collect()
    }

    fn problems(name: &str, source: &str) -> Vec<(GraphDiagnosticKind, String)> {
        requests(name, source).problems.into_iter().collect()
    }

    #[test]
    fn static_imports_and_side_effect_imports() {
        let source = r#"
            import a from "./a";
            import { b } from './b';
            import * as c from "../c";
            import "./side-effect";
            import data from "./data.json" with { type: "json" };
        "#;
        assert_eq!(
            specifiers("m.ts", source),
            ["../c", "./a", "./b", "./data.json", "./side-effect"]
        );
    }

    /// G03: re-exports and barrels are dependencies.
    #[test]
    fn g03_re_exports_are_dependencies() {
        let source = r#"
            export { a } from "./a";
            export * from "./b";
            export * as ns from "./c";
            export type { T } from "./types";
            export const local = 1;
        "#;
        assert_eq!(
            specifiers("index.ts", source),
            ["./a", "./b", "./c", "./types"]
        );
    }

    /// G04: type-only imports are kept on the safe side.
    #[test]
    fn g04_type_only_imports_are_dependencies() {
        let source = r#"
            import type { A } from "./a";
            import { type B, c } from "./b";
            type C = import("./c").C;
            import d = require("./d");
            export type { E } from "./e";
        "#;
        assert_eq!(
            specifiers("m.ts", source),
            ["./a", "./b", "./c", "./d", "./e"]
        );
    }

    /// G05: literal dynamic imports and requires are followed.
    #[test]
    fn g05_literal_dynamic_import_and_require() {
        let source = r#"
            const a = await import("./a");
            const b = require("./b");
            const c = require(`./c`);
            const d = import(`./d`);
            const e = require.resolve("./e");
            const f = import.meta.resolve("./f");
            const g = (require as any)("./g");
            function lazy() { return import("./h"); }
        "#;
        assert_eq!(
            specifiers("m.ts", source),
            ["./a", "./b", "./c", "./d", "./e", "./f", "./g", "./h"]
        );
    }

    /// G06: non-literal dynamic imports are uncertainty, not "no dependency".
    #[test]
    fn g06_non_literal_dynamic_import_is_reported() {
        assert_eq!(
            problems("m.ts", "const m = await import(name);"),
            [(
                GraphDiagnosticKind::DynamicDependency,
                "import(<expression>)".to_owned()
            )]
        );
        assert_eq!(
            problems("m.ts", "const m = import(`./locales/${lang}.json`);"),
            [(
                GraphDiagnosticKind::DynamicDependency,
                "import(<expression>)".to_owned()
            )]
        );
        assert_eq!(
            problems("m.js", "const m = require(path.join(__dirname, name));"),
            [(
                GraphDiagnosticKind::DynamicDependency,
                "require(<expression>)".to_owned()
            )]
        );
        assert_eq!(
            problems("m.js", "const m = require(...names);"),
            [(
                GraphDiagnosticKind::DynamicDependency,
                "require(<expression>)".to_owned()
            )]
        );
    }

    #[test]
    fn literal_requests_survive_next_to_dynamic_ones() {
        let requests = requests("m.ts", r#"import "./a"; const b = import(name);"#);
        assert_eq!(requests.specifiers, BTreeSet::from(["./a".to_owned()]));
        assert_eq!(requests.problems.len(), 1);
    }

    #[test]
    fn unsupported_mechanisms_are_reported() {
        assert_eq!(
            problems("m.ts", "const pages = import.meta.glob('./pages/*.tsx');"),
            [(
                GraphDiagnosticKind::UnsupportedMechanism,
                "import.meta.glob".to_owned()
            )]
        );
        assert_eq!(
            problems("m.js", "const ctx = require.context('./x', true);"),
            [(
                GraphDiagnosticKind::UnsupportedMechanism,
                "require.context".to_owned()
            )]
        );
    }

    /// G07: a file that does not parse has unknown imports.
    #[test]
    fn g07_syntax_errors_are_parse_failures() {
        assert_eq!(
            problems("m.ts", "import { a from './a';"),
            [(GraphDiagnosticKind::ParseFailure, "syntax error".to_owned())]
        );
        assert_eq!(
            extract(&RepoPath::try_new("m.ts").unwrap(), b"import './a';\xff"),
            ModuleRequests::problem(
                GraphDiagnosticKind::ParseFailure,
                "the file is not valid UTF-8"
            )
        );
    }

    #[test]
    fn jsx_and_tsx_are_parsed() {
        let source = r#"
            import { Button } from "./Button";
            export const App = () => <Button label="x" />;
        "#;
        assert_eq!(specifiers("App.tsx", source), ["./Button"]);
        assert_eq!(specifiers("App.jsx", source), ["./Button"]);
        // JSX inside a plain .js file is accepted on the second attempt.
        assert_eq!(specifiers("App.js", source), ["./Button"]);
    }

    #[test]
    fn typescript_syntax_in_ts_files() {
        let source = r#"
            import { z } from "zod";
            enum Kind { A, B }
            export function f<T extends object>(value: T): asserts value is T {}
            const x = <T,>(v: T) => v;
            declare module "./augmented" { interface X { y: number } }
        "#;
        assert_eq!(specifiers("m.ts", source), ["zod"]);
    }

    #[test]
    fn commonjs_and_esm_extensions() {
        assert_eq!(
            specifiers("m.cjs", "module.exports = require('./a');"),
            ["./a"]
        );
        assert_eq!(
            specifiers("m.mjs", "export { a } from './a.mjs';"),
            ["./a.mjs"]
        );
        assert_eq!(
            specifiers("m.cts", "import a = require('./a'); export = a;"),
            ["./a"]
        );
        assert_eq!(
            specifiers("m.mts", "import { a } from './a.mjs';"),
            ["./a.mjs"]
        );
        assert_eq!(
            specifiers("types.d.ts", "export type { A } from './a';"),
            ["./a"]
        );
    }

    #[test]
    fn module_mocks_are_dependencies() {
        let source = r#"
            import { vi, mock } from "tool";
            vi.mock("./a");
            vi.mock("./b", () => ({ b: 1 }));
            jest.mock("./c");
            vi.importActual("./d");
            mock.module("./e", () => ({}));
            vi.mock(import("./f"), () => ({}));
        "#;
        assert_eq!(
            specifiers("m.test.ts", source),
            ["./a", "./b", "./c", "./d", "./e", "./f", "tool"]
        );
        assert_eq!(
            problems("m.test.ts", "vi.mock(modulePath);"),
            [(
                GraphDiagnosticKind::DynamicDependency,
                "module mock with a non-literal module".to_owned()
            )]
        );
    }

    #[test]
    fn strings_that_are_not_requests_are_ignored() {
        let source = r#"
            const text = "import './not-an-import'";
            // import "./commented";
            /* require("./commented-too") */
            const other = somethingElse("./x");
            obj.require("./method");
            require();
        "#;
        assert!(specifiers("m.js", source).is_empty());
    }

    #[test]
    fn a_file_without_requests_is_fine() {
        assert_eq!(
            requests("m.ts", "export const a = 1;"),
            ModuleRequests::default()
        );
        assert_eq!(requests("empty.ts", ""), ModuleRequests::default());
    }
}
