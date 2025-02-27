// Copyright 2018-2024 the Deno authors. All rights reserved. MIT license.

use std::collections::HashMap;
use std::io::IsTerminal;
use std::rc::Rc;
use std::sync::Arc;

use base64::prelude::BASE64_STANDARD;
use base64::Engine;
use deno_config::ConfigFile;
use deno_config::WorkspaceMemberConfig;
use deno_core::anyhow::bail;
use deno_core::anyhow::Context;
use deno_core::error::AnyError;
use deno_core::futures::FutureExt;
use deno_core::serde_json;
use deno_core::serde_json::json;
use deno_core::serde_json::Value;
use deno_core::unsync::JoinSet;
use deno_runtime::deno_fetch::reqwest;
use deno_terminal::colors;
use import_map::ImportMap;
use lsp_types::Url;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;

use crate::args::jsr_api_url;
use crate::args::jsr_url;
use crate::args::CliOptions;
use crate::args::Flags;
use crate::args::PublishFlags;
use crate::args::TypeCheckMode;
use crate::cache::LazyGraphSourceParser;
use crate::cache::ParsedSourceCache;
use crate::factory::CliFactory;
use crate::graph_util::ModuleGraphCreator;
use crate::http_util::HttpClient;
use crate::resolver::MappedSpecifierResolver;
use crate::resolver::SloppyImportsResolver;
use crate::tools::check::CheckOptions;
use crate::tools::lint::no_slow_types;
use crate::tools::registry::diagnostics::PublishDiagnostic;
use crate::tools::registry::diagnostics::PublishDiagnosticsCollector;
use crate::tools::registry::graph::collect_invalid_external_imports;
use crate::util::display::human_size;

mod api;
mod auth;
mod diagnostics;
mod graph;
mod paths;
mod pm;
mod provenance;
mod publish_order;
mod tar;
mod unfurl;

use auth::get_auth_method;
use auth::AuthMethod;
pub use pm::add;
use publish_order::PublishOrderGraph;
pub use unfurl::deno_json_deps;
use unfurl::SpecifierUnfurler;

use super::check::TypeChecker;

use self::tar::PublishableTarball;

fn ring_bell() {
  // ASCII code for the bell character.
  print!("\x07");
}

struct PreparedPublishPackage {
  scope: String,
  package: String,
  version: String,
  tarball: PublishableTarball,
  config: String,
  exports: HashMap<String, String>,
}

impl PreparedPublishPackage {
  pub fn display_name(&self) -> String {
    format!("@{}/{}@{}", self.scope, self.package, self.version)
  }
}

static SUGGESTED_ENTRYPOINTS: [&str; 4] =
  ["mod.ts", "mod.js", "index.ts", "index.js"];

#[allow(clippy::too_many_arguments)]
async fn prepare_publish(
  package_name: &str,
  deno_json: &ConfigFile,
  source_cache: Arc<ParsedSourceCache>,
  graph: Arc<deno_graph::ModuleGraph>,
  mapped_resolver: Arc<MappedSpecifierResolver>,
  sloppy_imports_resolver: Option<SloppyImportsResolver>,
  bare_node_builtins: bool,
  diagnostics_collector: &PublishDiagnosticsCollector,
) -> Result<Rc<PreparedPublishPackage>, AnyError> {
  let config_path = deno_json.specifier.to_file_path().unwrap();
  let dir_path = config_path.parent().unwrap().to_path_buf();
  let Some(version) = deno_json.json.version.clone() else {
    bail!("{} is missing 'version' field", deno_json.specifier);
  };
  if deno_json.json.exports.is_none() {
    let mut suggested_entrypoint = None;

    for entrypoint in SUGGESTED_ENTRYPOINTS {
      if dir_path.join(entrypoint).exists() {
        suggested_entrypoint = Some(entrypoint);
        break;
      }
    }

    let exports_content = format!(
      r#"{{
  "name": "{}",
  "version": "{}",
  "exports": "{}"
}}"#,
      package_name,
      version,
      suggested_entrypoint.unwrap_or("<path_to_entrypoint>")
    );

    bail!(
      "You did not specify an entrypoint to \"{}\" package in {}. Add `exports` mapping in the configuration file, eg:\n{}",
      package_name,
      deno_json.specifier,
      exports_content
    );
  }
  let Some(name_no_at) = package_name.strip_prefix('@') else {
    bail!("Invalid package name, use '@<scope_name>/<package_name> format");
  };
  let Some((scope, name_no_scope)) = name_no_at.split_once('/') else {
    bail!("Invalid package name, use '@<scope_name>/<package_name> format");
  };
  let file_patterns = deno_json.to_publish_config()?.map(|c| c.files);

  let diagnostics_collector = diagnostics_collector.clone();
  let tarball = deno_core::unsync::spawn_blocking(move || {
    let unfurler = SpecifierUnfurler::new(
      &mapped_resolver,
      sloppy_imports_resolver.as_ref(),
      bare_node_builtins,
    );
    tar::create_gzipped_tarball(
      &dir_path,
      LazyGraphSourceParser::new(&source_cache, &graph),
      &diagnostics_collector,
      &unfurler,
      file_patterns,
    )
    .context("Failed to create a tarball")
  })
  .await??;

  log::debug!("Tarball size ({}): {}", package_name, tarball.bytes.len());

  Ok(Rc::new(PreparedPublishPackage {
    scope: scope.to_string(),
    package: name_no_scope.to_string(),
    version: version.to_string(),
    tarball,
    exports: match &deno_json.json.exports {
      Some(Value::Object(exports)) => exports
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.as_str().unwrap().to_string()))
        .collect(),
      Some(Value::String(exports)) => {
        let mut map = HashMap::new();
        map.insert(".".to_string(), exports.to_string());
        map
      }
      _ => HashMap::new(),
    },
    // the config file is always at the root of a publishing dir,
    // so getting the file name is always correct
    config: config_path
      .file_name()
      .unwrap()
      .to_string_lossy()
      .to_string(),
  }))
}

#[derive(Serialize)]
#[serde(tag = "permission")]
pub enum Permission<'s> {
  #[serde(rename = "package/publish", rename_all = "camelCase")]
  VersionPublish {
    scope: &'s str,
    package: &'s str,
    version: &'s str,
    tarball_hash: &'s str,
  },
}

async fn get_auth_headers(
  client: &reqwest::Client,
  registry_url: String,
  packages: Vec<Rc<PreparedPublishPackage>>,
  auth_method: AuthMethod,
) -> Result<HashMap<(String, String, String), Rc<str>>, AnyError> {
  let permissions = packages
    .iter()
    .map(|package| Permission::VersionPublish {
      scope: &package.scope,
      package: &package.package,
      version: &package.version,
      tarball_hash: &package.tarball.hash,
    })
    .collect::<Vec<_>>();

  let mut authorizations = HashMap::with_capacity(packages.len());

  match auth_method {
    AuthMethod::Interactive => {
      let verifier = uuid::Uuid::new_v4().to_string();
      let challenge = BASE64_STANDARD.encode(sha2::Sha256::digest(&verifier));

      let response = client
        .post(format!("{}authorizations", registry_url))
        .json(&serde_json::json!({
          "challenge": challenge,
          "permissions": permissions,
        }))
        .send()
        .await
        .context("Failed to create interactive authorization")?;
      let auth =
        api::parse_response::<api::CreateAuthorizationResponse>(response)
          .await
          .context("Failed to create interactive authorization")?;

      let auth_url = format!("{}?code={}", auth.verification_url, auth.code);
      print!(
        "Visit {} to authorize publishing of",
        colors::cyan(&auth_url)
      );
      if packages.len() > 1 {
        println!(" {} packages", packages.len());
      } else {
        println!(" @{}/{}", packages[0].scope, packages[0].package);
      }

      ring_bell();
      println!("{}", colors::gray("Waiting..."));
      let _ = open::that_detached(&auth_url);

      let interval = std::time::Duration::from_secs(auth.poll_interval);

      loop {
        tokio::time::sleep(interval).await;
        let response = client
          .post(format!("{}authorizations/exchange", registry_url))
          .json(&serde_json::json!({
            "exchangeToken": auth.exchange_token,
            "verifier": verifier,
          }))
          .send()
          .await
          .context("Failed to exchange authorization")?;
        let res =
          api::parse_response::<api::ExchangeAuthorizationResponse>(response)
            .await;
        match res {
          Ok(res) => {
            println!(
              "{} {} {}",
              colors::green("Authorization successful."),
              colors::gray("Authenticated as"),
              colors::cyan(res.user.name)
            );
            let authorization: Rc<str> = format!("Bearer {}", res.token).into();
            for pkg in &packages {
              authorizations.insert(
                (pkg.scope.clone(), pkg.package.clone(), pkg.version.clone()),
                authorization.clone(),
              );
            }
            break;
          }
          Err(err) => {
            if err.code == "authorizationPending" {
              continue;
            } else {
              return Err(err).context("Failed to exchange authorization");
            }
          }
        }
      }
    }
    AuthMethod::Token(token) => {
      let authorization: Rc<str> = format!("Bearer {}", token).into();
      for pkg in &packages {
        authorizations.insert(
          (pkg.scope.clone(), pkg.package.clone(), pkg.version.clone()),
          authorization.clone(),
        );
      }
    }
    AuthMethod::Oidc(oidc_config) => {
      let mut chunked_packages = packages.chunks(16);
      for permissions in permissions.chunks(16) {
        let audience = json!({ "permissions": permissions }).to_string();
        let url = format!(
          "{}&audience={}",
          oidc_config.url,
          percent_encoding::percent_encode(
            audience.as_bytes(),
            percent_encoding::NON_ALPHANUMERIC
          )
        );

        let response = client
          .get(url)
          .bearer_auth(&oidc_config.token)
          .send()
          .await
          .context("Failed to get OIDC token")?;
        let status = response.status();
        let text = response.text().await.with_context(|| {
          format!("Failed to get OIDC token: status {}", status)
        })?;
        if !status.is_success() {
          bail!(
            "Failed to get OIDC token: status {}, response: '{}'",
            status,
            text
          );
        }
        let api::OidcTokenResponse { value } = serde_json::from_str(&text)
          .with_context(|| {
            format!(
              "Failed to parse OIDC token: '{}' (status {})",
              text, status
            )
          })?;

        let authorization: Rc<str> = format!("githuboidc {}", value).into();
        for pkg in chunked_packages.next().unwrap() {
          authorizations.insert(
            (pkg.scope.clone(), pkg.package.clone(), pkg.version.clone()),
            authorization.clone(),
          );
        }
      }
    }
  };

  Ok(authorizations)
}

/// Check if both `scope` and `package` already exist, if not return
/// a URL to the management panel to create them.
async fn check_if_scope_and_package_exist(
  client: &reqwest::Client,
  registry_api_url: &str,
  registry_manage_url: &str,
  scope: &str,
  package: &str,
) -> Result<Option<String>, AnyError> {
  let mut needs_scope = false;
  let mut needs_package = false;

  let response = api::get_scope(client, registry_api_url, scope).await?;
  if response.status() == 404 {
    needs_scope = true;
  }

  let response =
    api::get_package(client, registry_api_url, scope, package).await?;
  if response.status() == 404 {
    needs_package = true;
  }

  if needs_scope || needs_package {
    let create_url = format!(
      "{}new?scope={}&package={}&from=cli",
      registry_manage_url, scope, package
    );
    return Ok(Some(create_url));
  }

  Ok(None)
}

async fn ensure_scopes_and_packages_exist(
  client: &reqwest::Client,
  registry_api_url: String,
  registry_manage_url: String,
  packages: Vec<Rc<PreparedPublishPackage>>,
) -> Result<(), AnyError> {
  if !std::io::stdin().is_terminal() {
    let mut missing_packages_lines = vec![];
    for package in packages {
      let maybe_create_package_url = check_if_scope_and_package_exist(
        client,
        &registry_api_url,
        &registry_manage_url,
        &package.scope,
        &package.package,
      )
      .await?;

      if let Some(create_package_url) = maybe_create_package_url {
        missing_packages_lines.push(format!(" - {}", create_package_url));
      }
    }

    if !missing_packages_lines.is_empty() {
      bail!(
        "Following packages don't exist, follow the links and create them:\n{}",
        missing_packages_lines.join("\n")
      );
    }
    return Ok(());
  }

  for package in packages {
    let maybe_create_package_url = check_if_scope_and_package_exist(
      client,
      &registry_api_url,
      &registry_manage_url,
      &package.scope,
      &package.package,
    )
    .await?;

    let Some(create_package_url) = maybe_create_package_url else {
      continue;
    };

    ring_bell();
    println!(
      "'@{}/{}' doesn't exist yet. Visit {} to create the package",
      &package.scope,
      &package.package,
      colors::cyan_with_underline(&create_package_url)
    );
    println!("{}", colors::gray("Waiting..."));
    let _ = open::that_detached(&create_package_url);

    let package_api_url = api::get_package_api_url(
      &registry_api_url,
      &package.scope,
      &package.package,
    );

    loop {
      tokio::time::sleep(std::time::Duration::from_secs(3)).await;
      let response = client.get(&package_api_url).send().await?;
      if response.status() == 200 {
        let name = format!("@{}/{}", package.scope, package.package);
        println!("Package {} created", colors::green(name));
        break;
      }
    }
  }

  Ok(())
}

async fn perform_publish(
  http_client: &Arc<HttpClient>,
  mut publish_order_graph: PublishOrderGraph,
  mut prepared_package_by_name: HashMap<String, Rc<PreparedPublishPackage>>,
  auth_method: AuthMethod,
  no_provenance: bool,
) -> Result<(), AnyError> {
  let client = http_client.client()?;
  let registry_api_url = jsr_api_url().to_string();
  let registry_url = jsr_url().to_string();

  let packages = prepared_package_by_name
    .values()
    .cloned()
    .collect::<Vec<_>>();

  ensure_scopes_and_packages_exist(
    client,
    registry_api_url.clone(),
    registry_url.clone(),
    packages.clone(),
  )
  .await?;

  let mut authorizations =
    get_auth_headers(client, registry_api_url.clone(), packages, auth_method)
      .await?;

  assert_eq!(prepared_package_by_name.len(), authorizations.len());
  let mut futures: JoinSet<Result<String, AnyError>> = JoinSet::default();
  loop {
    let next_batch = publish_order_graph.next();

    for package_name in next_batch {
      let package = prepared_package_by_name.remove(&package_name).unwrap();

      // todo(dsherret): output something that looks better than this even not in debug
      if log::log_enabled!(log::Level::Debug) {
        log::debug!("Publishing {}", package.display_name());
        for file in &package.tarball.files {
          log::debug!(
            "  Tarball file {} {}",
            human_size(file.size as f64),
            file.specifier
          );
        }
      }

      let authorization = authorizations
        .remove(&(
          package.scope.clone(),
          package.package.clone(),
          package.version.clone(),
        ))
        .unwrap();
      let registry_api_url = registry_api_url.clone();
      let registry_url = registry_url.clone();
      let http_client = http_client.clone();
      futures.spawn(async move {
        let display_name = package.display_name();
        publish_package(
          &http_client,
          package,
          &registry_api_url,
          &registry_url,
          &authorization,
          no_provenance,
        )
        .await
        .with_context(|| format!("Failed to publish {}", display_name))?;
        Ok(package_name)
      });
    }

    let Some(result) = futures.join_next().await else {
      // done, ensure no circular dependency
      publish_order_graph.ensure_no_pending()?;
      break;
    };

    let package_name = result??;
    publish_order_graph.finish_package(&package_name);
  }

  Ok(())
}

async fn publish_package(
  http_client: &HttpClient,
  package: Rc<PreparedPublishPackage>,
  registry_api_url: &str,
  registry_url: &str,
  authorization: &str,
  no_provenance: bool,
) -> Result<(), AnyError> {
  let client = http_client.client()?;
  println!(
    "{} @{}/{}@{} ...",
    colors::intense_blue("Publishing"),
    package.scope,
    package.package,
    package.version
  );

  let url = format!(
    "{}scopes/{}/packages/{}/versions/{}?config=/{}",
    registry_api_url,
    package.scope,
    package.package,
    package.version,
    package.config
  );

  let response = client
    .post(url)
    .header(reqwest::header::AUTHORIZATION, authorization)
    .header(reqwest::header::CONTENT_ENCODING, "gzip")
    .body(package.tarball.bytes.clone())
    .send()
    .await?;

  let res = api::parse_response::<api::PublishingTask>(response).await;
  let mut task = match res {
    Ok(task) => task,
    Err(mut err) if err.code == "duplicateVersionPublish" => {
      let task = serde_json::from_value::<api::PublishingTask>(
        err.data.get_mut("task").unwrap().take(),
      )
      .unwrap();
      if task.status == "success" {
        println!(
          "{} @{}/{}@{}",
          colors::yellow("Warning: Skipping, already published"),
          package.scope,
          package.package,
          package.version
        );
        return Ok(());
      }
      println!(
        "{} @{}/{}@{}",
        colors::yellow("Already uploaded, waiting for publishing"),
        package.scope,
        package.package,
        package.version
      );
      task
    }
    Err(err) => {
      return Err(err).with_context(|| {
        format!(
          "Failed to publish @{}/{} at {}",
          package.scope, package.package, package.version
        )
      })
    }
  };

  let interval = std::time::Duration::from_secs(2);
  while task.status != "success" && task.status != "failure" {
    tokio::time::sleep(interval).await;
    let resp = client
      .get(format!("{}publish_status/{}", registry_api_url, task.id))
      .send()
      .await
      .with_context(|| {
        format!(
          "Failed to get publishing status for @{}/{} at {}",
          package.scope, package.package, package.version
        )
      })?;
    task = api::parse_response::<api::PublishingTask>(resp)
      .await
      .with_context(|| {
        format!(
          "Failed to get publishing status for @{}/{} at {}",
          package.scope, package.package, package.version
        )
      })?;
  }

  if let Some(error) = task.error {
    bail!(
      "{} @{}/{} at {}: {}",
      colors::red("Failed to publish"),
      package.scope,
      package.package,
      package.version,
      error.message
    );
  }

  println!(
    "{} @{}/{}@{}",
    colors::green("Successfully published"),
    package.scope,
    package.package,
    package.version
  );

  let enable_provenance = std::env::var("DISABLE_JSR_PROVENANCE").is_err()
    || (auth::is_gha() && auth::gha_oidc_token().is_some() && !no_provenance);

  // Enable provenance by default on Github actions with OIDC token
  if enable_provenance {
    // Get the version manifest from the registry
    let meta_url = jsr_url().join(&format!(
      "@{}/{}/{}_meta.json",
      package.scope, package.package, package.version
    ))?;

    let meta_bytes = client.get(meta_url).send().await?.bytes().await?;

    if std::env::var("DISABLE_JSR_MANIFEST_VERIFICATION_FOR_TESTING").is_err() {
      verify_version_manifest(&meta_bytes, &package)?;
    }

    let subject = provenance::Subject {
      name: format!(
        "pkg:jsr/@{}/{}@{}",
        package.scope, package.package, package.version
      ),
      digest: provenance::SubjectDigest {
        sha256: hex::encode(sha2::Sha256::digest(&meta_bytes)),
      },
    };
    let bundle = provenance::generate_provenance(subject).await?;

    let tlog_entry = &bundle.verification_material.tlog_entries[0];
    println!("{}",
      colors::green(format!(
        "Provenance transparency log available at https://search.sigstore.dev/?logIndex={}",
        tlog_entry.log_index
      ))
     );

    // Submit bundle to JSR
    let provenance_url = format!(
      "{}scopes/{}/packages/{}/versions/{}/provenance",
      registry_api_url, package.scope, package.package, package.version
    );
    client
      .post(provenance_url)
      .header(reqwest::header::AUTHORIZATION, authorization)
      .json(&json!({ "bundle": bundle }))
      .send()
      .await?;
  }

  println!(
    "{}",
    colors::gray(format!(
      "Visit {}@{}/{}@{} for details",
      registry_url, package.scope, package.package, package.version
    ))
  );
  Ok(())
}

struct PreparePackagesData {
  publish_order_graph: PublishOrderGraph,
  package_by_name: HashMap<String, Rc<PreparedPublishPackage>>,
}

async fn prepare_packages_for_publishing(
  cli_factory: &CliFactory,
  allow_slow_types: bool,
  diagnostics_collector: &PublishDiagnosticsCollector,
  deno_json: ConfigFile,
  mapped_resolver: Arc<MappedSpecifierResolver>,
) -> Result<PreparePackagesData, AnyError> {
  let members = deno_json.to_workspace_members()?;
  let module_graph_creator = cli_factory.module_graph_creator().await?.as_ref();
  let source_cache = cli_factory.parsed_source_cache();
  let type_checker = cli_factory.type_checker().await?;
  let fs = cli_factory.fs();
  let cli_options = cli_factory.cli_options();
  let bare_node_builtins = cli_options.unstable_bare_node_builtins();

  if members.len() > 1 {
    println!("Publishing a workspace...");
  }

  // create the module graph
  let graph = build_and_check_graph_for_publish(
    module_graph_creator,
    type_checker,
    cli_options,
    allow_slow_types,
    diagnostics_collector,
    &members,
  )
  .await?;

  let mut package_by_name = HashMap::with_capacity(members.len());
  let publish_order_graph =
    publish_order::build_publish_order_graph(&graph, &members)?;

  let results = members
    .into_iter()
    .map(|member| {
      let mapped_resolver = mapped_resolver.clone();
      let sloppy_imports_resolver = if cli_options.unstable_sloppy_imports() {
        Some(SloppyImportsResolver::new(fs.clone()))
      } else {
        None
      };
      let graph = graph.clone();
      async move {
        let package = prepare_publish(
          &member.package_name,
          &member.config_file,
          source_cache.clone(),
          graph,
          mapped_resolver,
          sloppy_imports_resolver,
          bare_node_builtins,
          diagnostics_collector,
        )
        .await
        .with_context(|| {
          format!("Failed preparing '{}'.", member.package_name)
        })?;
        Ok::<_, AnyError>((member.package_name, package))
      }
      .boxed()
    })
    .collect::<Vec<_>>();
  let results = deno_core::futures::future::join_all(results).await;
  for result in results {
    let (package_name, package) = result?;
    package_by_name.insert(package_name, package);
  }
  Ok(PreparePackagesData {
    publish_order_graph,
    package_by_name,
  })
}

async fn build_and_check_graph_for_publish(
  module_graph_creator: &ModuleGraphCreator,
  type_checker: &TypeChecker,
  cli_options: &CliOptions,
  allow_slow_types: bool,
  diagnostics_collector: &PublishDiagnosticsCollector,
  packages: &[WorkspaceMemberConfig],
) -> Result<Arc<deno_graph::ModuleGraph>, deno_core::anyhow::Error> {
  let graph = module_graph_creator.create_publish_graph(packages).await?;
  graph.valid()?;

  // todo(dsherret): move to lint rule
  collect_invalid_external_imports(&graph, diagnostics_collector);

  if allow_slow_types {
    log::info!(
      concat!(
        "{} Publishing a library with slow types is not recommended. ",
        "This may lead to poor type checking performance for users of ",
        "your package, may affect the quality of automatic documentation ",
        "generation, and your package will not be shipped with a .d.ts ",
        "file for Node.js users."
      ),
      colors::yellow("Warning"),
    );
    Ok(Arc::new(graph))
  } else {
    log::info!("Checking for slow types in the public API...");
    let mut any_pkg_had_diagnostics = false;
    for package in packages {
      let export_urls = package.config_file.resolve_export_value_urls()?;
      let diagnostics =
        no_slow_types::collect_no_slow_type_diagnostics(&export_urls, &graph);
      if !diagnostics.is_empty() {
        any_pkg_had_diagnostics = true;
        for diagnostic in diagnostics {
          diagnostics_collector.push(PublishDiagnostic::FastCheck(diagnostic));
        }
      }
    }

    if any_pkg_had_diagnostics {
      Ok(Arc::new(graph))
    } else {
      // fast check passed, type check the output as a temporary measure
      // until we know that it's reliable and stable
      let (graph, check_diagnostics) = type_checker
        .check_diagnostics(
          graph,
          CheckOptions {
            build_fast_check_graph: false, // already built
            lib: cli_options.ts_type_lib_window(),
            log_ignored_options: false,
            reload: cli_options.reload_flag(),
            // force type checking this
            type_check_mode: TypeCheckMode::Local,
          },
        )
        .await?;
      if !check_diagnostics.is_empty() {
        bail!(
          concat!(
            "Failed ensuring public API type output is valid.\n\n",
            "{:#}\n\n",
            "You may have discovered a bug in Deno. Please open an issue at: ",
            "https://github.com/denoland/deno/issues/"
          ),
          check_diagnostics
        );
      }
      Ok(graph)
    }
  }
}

pub async fn publish(
  flags: Flags,
  publish_flags: PublishFlags,
) -> Result<(), AnyError> {
  let cli_factory = CliFactory::from_flags(flags).await?;

  let auth_method = get_auth_method(publish_flags.token)?;

  let import_map = cli_factory
    .maybe_import_map()
    .await?
    .clone()
    .unwrap_or_else(|| {
      Arc::new(ImportMap::new(Url::parse("file:///dev/null").unwrap()))
    });

  let directory_path = cli_factory.cli_options().initial_cwd();

  let mapped_resolver = Arc::new(MappedSpecifierResolver::new(
    Some(import_map),
    cli_factory.package_json_deps_provider().clone(),
  ));
  let cli_options = cli_factory.cli_options();
  let Some(config_file) = cli_options.maybe_config_file() else {
    bail!(
      "Couldn't find a deno.json, deno.jsonc, jsr.json or jsr.jsonc configuration file in {}.",
      directory_path.display()
    );
  };

  let diagnostics_collector = PublishDiagnosticsCollector::default();

  let prepared_data = prepare_packages_for_publishing(
    &cli_factory,
    publish_flags.allow_slow_types,
    &diagnostics_collector,
    config_file.clone(),
    mapped_resolver,
  )
  .await?;

  diagnostics_collector.print_and_error()?;

  if prepared_data.package_by_name.is_empty() {
    bail!("No packages to publish");
  }

  if publish_flags.dry_run {
    for (_, package) in prepared_data.package_by_name {
      log::info!(
        "{} of {} with files:",
        colors::green_bold("Simulating publish"),
        colors::gray(package.display_name()),
      );
      for file in &package.tarball.files {
        log::info!("   {} ({})", file.specifier, human_size(file.size as f64),);
      }
    }
    log::warn!("{} Aborting due to --dry-run", colors::yellow("Warning"));
    return Ok(());
  }

  perform_publish(
    cli_factory.http_client(),
    prepared_data.publish_order_graph,
    prepared_data.package_by_name,
    auth_method,
    publish_flags.no_provenance,
  )
  .await?;

  Ok(())
}

#[derive(Deserialize)]
struct ManifestEntry {
  checksum: String,
}

#[derive(Deserialize)]
struct VersionManifest {
  manifest: HashMap<String, ManifestEntry>,
  exports: HashMap<String, String>,
}

fn verify_version_manifest(
  meta_bytes: &[u8],
  package: &PreparedPublishPackage,
) -> Result<(), AnyError> {
  let manifest = serde_json::from_slice::<VersionManifest>(meta_bytes)?;
  // Check that nothing was removed from the manifest.
  if manifest.manifest.len() != package.tarball.files.len() {
    bail!(
      "Mismatch in the number of files in the manifest: expected {}, got {}",
      package.tarball.files.len(),
      manifest.manifest.len()
    );
  }

  for (path, entry) in manifest.manifest {
    // Verify each path with the files in the tarball.
    let file = package
      .tarball
      .files
      .iter()
      .find(|f| f.path_str == path.as_str());

    if let Some(file) = file {
      if file.hash != entry.checksum {
        bail!(
          "Checksum mismatch for {}: expected {}, got {}",
          path,
          entry.checksum,
          file.hash
        );
      }
    } else {
      bail!("File {} not found in the tarball", path);
    }
  }

  for (specifier, expected) in &manifest.exports {
    let actual = package.exports.get(specifier).ok_or_else(|| {
      deno_core::anyhow::anyhow!(
        "Export {} not found in the package",
        specifier
      )
    })?;
    if actual != expected {
      bail!(
        "Export {} mismatch: expected {}, got {}",
        specifier,
        expected,
        actual
      );
    }
  }

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::tar::PublishableTarball;
  use super::tar::PublishableTarballFile;
  use super::verify_version_manifest;
  use std::collections::HashMap;

  #[test]
  fn test_verify_version_manifest() {
    let meta = r#"{
      "manifest": {
        "mod.ts": {
          "checksum": "abc123"
        }
      },
      "exports": {}
    }"#;

    let meta_bytes = meta.as_bytes();
    let package = super::PreparedPublishPackage {
      scope: "test".to_string(),
      package: "test".to_string(),
      version: "1.0.0".to_string(),
      tarball: PublishableTarball {
        bytes: vec![].into(),
        hash: "abc123".to_string(),
        files: vec![PublishableTarballFile {
          specifier: "file://mod.ts".try_into().unwrap(),
          path_str: "mod.ts".to_string(),
          hash: "abc123".to_string(),
          size: 0,
        }],
      },
      config: "deno.json".to_string(),
      exports: HashMap::new(),
    };

    assert!(verify_version_manifest(meta_bytes, &package).is_ok());
  }

  #[test]
  fn test_verify_version_manifest_missing() {
    let meta = r#"{
      "manifest": {
        "mod.ts": {},
      },
      "exports": {}
    }"#;

    let meta_bytes = meta.as_bytes();
    let package = super::PreparedPublishPackage {
      scope: "test".to_string(),
      package: "test".to_string(),
      version: "1.0.0".to_string(),
      tarball: PublishableTarball {
        bytes: vec![].into(),
        hash: "abc123".to_string(),
        files: vec![PublishableTarballFile {
          specifier: "file://mod.ts".try_into().unwrap(),
          path_str: "mod.ts".to_string(),
          hash: "abc123".to_string(),
          size: 0,
        }],
      },
      config: "deno.json".to_string(),
      exports: HashMap::new(),
    };

    assert!(verify_version_manifest(meta_bytes, &package).is_err());
  }

  #[test]
  fn test_verify_version_manifest_invalid_hash() {
    let meta = r#"{
      "manifest": {
        "mod.ts": {
          "checksum": "lol123"
        },
        "exports": {}
      }
    }"#;

    let meta_bytes = meta.as_bytes();
    let package = super::PreparedPublishPackage {
      scope: "test".to_string(),
      package: "test".to_string(),
      version: "1.0.0".to_string(),
      tarball: PublishableTarball {
        bytes: vec![].into(),
        hash: "abc123".to_string(),
        files: vec![PublishableTarballFile {
          specifier: "file://mod.ts".try_into().unwrap(),
          path_str: "mod.ts".to_string(),
          hash: "abc123".to_string(),
          size: 0,
        }],
      },
      config: "deno.json".to_string(),
      exports: HashMap::new(),
    };

    assert!(verify_version_manifest(meta_bytes, &package).is_err());
  }
}
