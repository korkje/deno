// Copyright 2018-2024 the Deno authors. All rights reserved. MIT license.

use deno_core::serde_json::json;
use test_util::assert_contains;
use test_util::assert_not_contains;
use test_util::env_vars_for_jsr_npm_tests;
use test_util::env_vars_for_jsr_provenance_tests;
use test_util::env_vars_for_jsr_tests;
use test_util::env_vars_for_npm_tests;
use test_util::itest;
use test_util::TestContextBuilder;

itest!(no_token {
  args: "publish",
  cwd: Some("publish/missing_deno_json"),
  output: "publish/no_token.out",
  exit_code: 1,
});

itest!(missing_deno_json {
  args: "publish --token 'sadfasdf'",
  output: "publish/missing_deno_json.out",
  cwd: Some("publish/missing_deno_json"),
  exit_code: 1,
});

itest!(has_slow_types {
  args: "publish --token 'sadfasdf'",
  output: "publish/has_slow_types.out",
  cwd: Some("publish/has_slow_types"),
  exit_code: 1,
});

itest!(allow_slow_types {
  args: "publish --allow-slow-types --token 'sadfasdf'",
  output: "publish/allow_slow_types.out",
  cwd: Some("publish/has_slow_types"),
  envs: env_vars_for_jsr_tests(),
  http_server: true,
  exit_code: 0,
});

itest!(invalid_path {
  args: "publish --token 'sadfasdf'",
  output: "publish/invalid_path.out",
  cwd: Some("publish/invalid_path"),
  exit_code: 1,
});

itest!(symlink {
  args: "publish --token 'sadfasdf' --dry-run",
  output: "publish/symlink.out",
  cwd: Some("publish/symlink"),
  exit_code: 0,
});

itest!(invalid_import {
  args: "publish --token 'sadfasdf' --dry-run",
  output: "publish/invalid_import.out",
  cwd: Some("publish/invalid_import"),
  envs: env_vars_for_npm_tests(),
  exit_code: 1,
  http_server: true,
});

#[test]
fn publish_non_exported_files_using_import_map() {
  let context = publish_context_builder().build();
  let temp_dir = context.temp_dir().path();
  temp_dir.join("deno.json").write_json(&json!({
    "name": "@foo/bar",
    "version": "1.0.0",
    "exports": "./mod.ts",
    "imports": {
      "@denotest/add": "jsr:@denotest/add@1"
    }
  }));
  // file not in the graph
  let other_ts = temp_dir.join("_other.ts");
  other_ts
    .write("import { add } from '@denotest/add'; console.log(add(1, 3));");
  let mod_ts = temp_dir.join("mod.ts");
  mod_ts.write("import { add } from '@denotest/add'; console.log(add(1, 2));");
  let output = context
    .new_command()
    .args("publish --log-level=debug --token 'sadfasdf'")
    .run();
  output.assert_exit_code(0);
  let lines = output.combined_output().split('\n').collect::<Vec<_>>();
  eprintln!("{}", output.combined_output());
  assert!(lines
    .iter()
    .any(|l| l.contains("Unfurling") && l.ends_with("mod.ts")));
  assert!(lines
    .iter()
    .any(|l| l.contains("Unfurling") && l.ends_with("other.ts")));
}

#[test]
fn publish_warning_not_in_graph() {
  let context = publish_context_builder().build();
  let temp_dir = context.temp_dir().path();
  temp_dir.join("deno.json").write_json(&json!({
    "name": "@foo/bar",
    "version": "1.0.0",
    "exports": "./mod.ts",
  }));
  // file not in the graph that uses a non-analyzable dynamic import (cause a diagnostic)
  let other_ts = temp_dir.join("_other.ts");
  other_ts
    .write("const nonAnalyzable = './_other.ts'; await import(nonAnalyzable);");
  let mod_ts = temp_dir.join("mod.ts");
  mod_ts.write(
    "export function test(a: number, b: number): number { return a + b; }",
  );
  context
    .new_command()
    .args("publish --token 'sadfasdf'")
    .run()
    .assert_matches_text(
      "[WILDCARD]unable to analyze dynamic import[WILDCARD]",
    );
}

itest!(javascript_missing_decl_file {
  args: "publish --token 'sadfasdf'",
  output: "publish/javascript_missing_decl_file.out",
  cwd: Some("publish/javascript_missing_decl_file"),
  envs: env_vars_for_jsr_tests(),
  exit_code: 0,
  http_server: true,
});

itest!(unanalyzable_dynamic_import {
  args: "publish --token 'sadfasdf'",
  output: "publish/unanalyzable_dynamic_import.out",
  cwd: Some("publish/unanalyzable_dynamic_import"),
  envs: env_vars_for_jsr_tests(),
  exit_code: 0,
  http_server: true,
});

itest!(javascript_decl_file {
  args: "publish --token 'sadfasdf'",
  output: "publish/javascript_decl_file.out",
  cwd: Some("publish/javascript_decl_file"),
  envs: env_vars_for_jsr_tests(),
  http_server: true,
  exit_code: 0,
});

itest!(package_json {
  args: "publish --token 'sadfasdf'",
  output: "publish/package_json.out",
  cwd: Some("publish/package_json"),
  envs: env_vars_for_jsr_npm_tests(),
  http_server: true,
});

itest!(successful {
  args: "publish --token 'sadfasdf'",
  output: "publish/successful.out",
  cwd: Some("publish/successful"),
  envs: env_vars_for_jsr_tests(),
  http_server: true,
});

itest!(provenance {
  args: "publish",
  output: "publish/successful_provenance.out",
  cwd: Some("publish/successful"),
  envs: env_vars_for_jsr_provenance_tests(),
  http_server: true,
});

itest!(no_check {
  args: "publish --token 'sadfasdf' --no-check",
  // still type checks the slow types output though
  output: "publish/successful_no_check.out",
  cwd: Some("publish/successful"),
  envs: env_vars_for_jsr_tests(),
  http_server: true,
});

itest!(node_specifier {
  args: "publish --token 'sadfasdf'",
  output: "publish/node_specifier.out",
  cwd: Some("publish/node_specifier"),
  envs: env_vars_for_jsr_tests()
    .into_iter()
    .chain(env_vars_for_npm_tests().into_iter())
    .collect(),
  http_server: true,
});

itest!(config_file_jsonc {
  args: "publish --token 'sadfasdf'",
  output: "publish/deno_jsonc.out",
  cwd: Some("publish/deno_jsonc"),
  envs: env_vars_for_jsr_tests(),
  http_server: true,
});

itest!(workspace_all {
  args: "publish --token 'sadfasdf'",
  output: "publish/workspace.out",
  cwd: Some("publish/workspace"),
  envs: env_vars_for_jsr_tests(),
  http_server: true,
});

itest!(workspace_individual {
  args: "publish --token 'sadfasdf'",
  output: "publish/workspace_individual.out",
  cwd: Some("publish/workspace/bar"),
  envs: env_vars_for_jsr_tests(),
  http_server: true,
});

itest!(dry_run {
  args: "publish --token 'sadfasdf' --dry-run",
  cwd: Some("publish/successful"),
  output: "publish/dry_run.out",
  envs: env_vars_for_jsr_tests(),
  http_server: true,
});

itest!(config_flag {
  args: "publish --token 'sadfasdf' --config=successful/deno.json",
  output: "publish/successful.out",
  cwd: Some("publish"),
  envs: env_vars_for_jsr_tests(),
  http_server: true,
});

itest!(bare_node_builtins {
  args: "publish --token 'sadfasdf' --dry-run --unstable-bare-node-builtins",
  output: "publish/bare_node_builtins.out",
  cwd: Some("publish/bare_node_builtins"),
  envs: env_vars_for_jsr_npm_tests(),
  http_server: true,
});

itest!(sloppy_imports {
  args: "publish --token 'sadfasdf' --dry-run --unstable-sloppy-imports",
  output: "publish/sloppy_imports.out",
  cwd: Some("publish/sloppy_imports"),
  envs: env_vars_for_jsr_tests(),
  http_server: true,
});

itest!(jsr_jsonc {
  args: "publish --token 'sadfasdf'",
  cwd: Some("publish/jsr_jsonc"),
  output: "publish/jsr_jsonc/mod.out",
  envs: env_vars_for_jsr_tests(),
  http_server: true,
});

itest!(unsupported_jsx_tsx {
  args: "publish --token 'sadfasdf'",
  cwd: Some("publish/unsupported_jsx_tsx"),
  output: "publish/unsupported_jsx_tsx/mod.out",
  envs: env_vars_for_jsr_npm_tests(),
  http_server: true,
});

#[test]
fn ignores_gitignore() {
  let context = publish_context_builder().build();
  let temp_dir = context.temp_dir().path();
  temp_dir.join("deno.json").write_json(&json!({
    "name": "@foo/bar",
    "version": "1.0.0",
    "exports": "./main.ts"
  }));

  temp_dir.join("main.ts").write("import './sub_dir/b.ts';");

  let gitignore = temp_dir.join(".gitignore");
  gitignore.write("ignored.ts\nsub_dir/ignored.wasm");

  let sub_dir = temp_dir.join("sub_dir");
  sub_dir.create_dir_all();
  sub_dir.join("ignored.wasm").write("");
  sub_dir.join("b.ts").write("export default {}");

  temp_dir.join("ignored.ts").write("");

  let output = context
    .new_command()
    .arg("publish")
    .arg("--dry-run")
    .arg("--token")
    .arg("sadfasdf")
    .run();
  output.assert_exit_code(0);
  let output = output.combined_output();
  assert_contains!(output, "b.ts");
  assert_contains!(output, "main.ts");
  assert_not_contains!(output, "ignored.ts");
  assert_not_contains!(output, "ignored.wasm");
}

#[test]
fn ignores_directories() {
  let context = publish_context_builder().build();
  let temp_dir = context.temp_dir().path();
  temp_dir.join("deno.json").write_json(&json!({
    "name": "@foo/bar",
    "version": "1.0.0",
    "exclude": [ "ignore" ],
    "publish": {
      "exclude": [ "ignore2" ]
    },
    "exports": "./main_included.ts"
  }));

  let ignored_dirs = vec![
    temp_dir.join(".git"),
    temp_dir.join("node_modules"),
    temp_dir.join("ignore"),
    temp_dir.join("ignore2"),
  ];
  for ignored_dir in ignored_dirs {
    ignored_dir.create_dir_all();
    ignored_dir.join("ignored.ts").write("");
  }

  let sub_dir = temp_dir.join("sub_dir");
  sub_dir.create_dir_all();
  sub_dir.join("sub_included.ts").write("");

  temp_dir.join("main_included.ts").write("");

  let output = context
    .new_command()
    .arg("publish")
    .arg("--log-level=debug")
    .arg("--token")
    .arg("sadfasdf")
    .run();
  output.assert_exit_code(0);
  let output = output.combined_output();
  assert_contains!(output, "sub_included.ts");
  assert_contains!(output, "main_included.ts");
  assert_not_contains!(output, "ignored.ts");
}

#[test]
fn includes_directories_with_gitignore() {
  let context = publish_context_builder().build();
  let temp_dir = context.temp_dir().path();
  temp_dir.join("deno.json").write_json(&json!({
    "name": "@foo/bar",
    "version": "1.0.0",
    "exports": "./main.ts",
    "publish": {
      "include": [ "deno.json", "main.ts" ]
    }
  }));

  temp_dir.join(".gitignore").write("main.ts");
  temp_dir.join("main.ts").write("");
  temp_dir.join("ignored.ts").write("");

  let output = context
    .new_command()
    .arg("publish")
    .arg("--token")
    .arg("sadfasdf")
    .run();
  output.assert_exit_code(0);
  let output = output.combined_output();
  assert_contains!(output, "main.ts");
  assert_not_contains!(output, "ignored.ts");
}

#[test]
fn includes_directories() {
  let context = publish_context_builder().build();
  let temp_dir = context.temp_dir().path();
  temp_dir.join("deno.json").write_json(&json!({
    "name": "@foo/bar",
    "version": "1.0.0",
    "exports": "./main.ts",
    "publish": {
      "include": [ "deno.json", "main.ts" ]
    }
  }));

  temp_dir.join("main.ts").write("");
  temp_dir.join("ignored.ts").write("");

  let output = context
    .new_command()
    .arg("publish")
    .arg("--token")
    .arg("sadfasdf")
    .run();
  output.assert_exit_code(0);
  let output = output.combined_output();
  assert_contains!(output, "main.ts");
  assert_not_contains!(output, "ignored.ts");
}

#[test]
fn includes_dotenv() {
  let context = publish_context_builder().build();
  let temp_dir = context.temp_dir().path();
  temp_dir.join("deno.json").write_json(&json!({
    "name": "@foo/bar",
    "version": "1.0.0",
    "exports": "./main.ts",
  }));

  temp_dir.join("main.ts").write("");
  temp_dir.join(".env").write("FOO=BAR");

  let output = context
    .new_command()
    .arg("publish")
    .arg("--token")
    .arg("sadfasdf")
    .arg("--dry-run")
    .run();
  output.assert_exit_code(0);
  let output = output.combined_output();
  assert_contains!(output, "main.ts");
  assert_not_contains!(output, ".env");
}

fn publish_context_builder() -> TestContextBuilder {
  TestContextBuilder::new()
    .use_http_server()
    .envs(env_vars_for_jsr_tests())
    .use_temp_cwd()
}
