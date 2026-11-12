//! `keep spec` — offline integration spec, and `spec gen` typed clients.

use std::path::PathBuf;

use anyhow::Result;

use clap::{Subcommand, ValueEnum};

/// `keep spec [--format ...] [--shapes] [--fields]` or `keep spec gen ...`.
/// Positional slots are reserved for the `gen` subcommand; everything else is a
/// flag (the CLI convention).
///
/// @spec .aw/tech-design/projects/keep/interfaces/cli/deploy-cli-keep-spec-spec-gen-dockerfile-render.md
#[derive(clap::Args, Debug)]
pub(crate) struct SpecArgs {
    /// Generate a typed client from the spec instead of printing it.
    #[command(subcommand)]
    gen: Option<SpecSub>,
    /// Schema format to emit when neither `--shapes` nor `--fields` is set.
    #[arg(long, value_enum, default_value_t = SpecFormat::Openapi)]
    format: SpecFormat,
    /// Emit the request-shape cookbook (canonical request bodies) instead.
    #[arg(long)]
    shapes: bool,
    /// Emit the value-type catalog (the KvValue kinds keep stores) instead.
    #[arg(long)]
    fields: bool,
}

#[derive(Subcommand, Debug)]
enum SpecSub {
    /// Generate a typed API client (TypeScript / Python / Rust) from keep's
    /// OpenAPI document, written into `--out`.
    Gen(GenArgs),
}

#[derive(clap::Args, Debug)]
struct GenArgs {
    /// Target language for the generated client.
    #[arg(long, value_enum)]
    lang: GenLang,
    /// Pinned generated-client contract, e.g. `python-3.14`. Defaults to
    /// `clients/codegen.toml`; an explicit value overrides that policy once.
    #[arg(long, value_name = "TARGET")]
    target: Option<String>,
    /// Output directory for the generated files.
    #[arg(long)]
    out: PathBuf,
    /// HTTP backend for the TypeScript client (ignored for py/rust).
    #[arg(long, value_enum, default_value_t = GenHttp::Fetch)]
    http: GenHttp,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum GenLang {
    /// TypeScript: types + fetch/axios client + TanStack Query hooks.
    Ts,
    /// Python: pydantic models + a generated sync/async HTTP/2 runtime.
    Py,
    /// Rust: serde models + a reqwest client.
    Rust,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum GenHttp {
    Fetch,
    Axios,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum SpecFormat {
    /// Full OpenAPI 3 document as JSON (default).
    Openapi,
    /// Full OpenAPI 3 document as YAML for LLM/agent reading.
    #[value(alias = "yaml", alias = "openapi.yaml")]
    OpenapiYaml,
    /// Just the component schemas (request/response data types).
    JsonSchema,
}

/// `keep spec` — offline OpenAPI / JSON-Schema / cookbook, or `spec gen` to
/// generate a typed client. No engine, no server, no I/O beyond stdout / `--out`.
///
/// @spec .aw/tech-design/projects/keep/interfaces/cli/deploy-cli-keep-spec-spec-gen-dockerfile-render.md
pub(crate) fn run(args: SpecArgs) -> Result<()> {
    // `spec gen` writes a typed client; everything else prints to stdout.
    if let Some(SpecSub::Gen(gen)) = args.gen {
        return spec_gen(gen);
    }
    let out = if args.shapes {
        serde_json::to_string_pretty(&keep::spec::request_shapes())?
    } else if args.fields {
        serde_json::to_string_pretty(&keep::spec::value_catalog())?
    } else {
        match args.format {
            SpecFormat::Openapi => keep::spec::openapi_json(),
            SpecFormat::OpenapiYaml => keep::spec::openapi_yaml(),
            SpecFormat::JsonSchema => keep::spec::json_schema_json(),
        }
    };
    println!("{out}");
    Ok(())
}

/// `keep spec gen` — generate a typed client from keep's own OpenAPI document
/// (offline; no engine or server) via the shared `libs/openapi-codegen`, written
/// into `--out`. One codegen path, no external tool.
///
/// @spec .aw/tech-design/projects/keep/interfaces/cli/deploy-cli-keep-spec-spec-gen-dockerfile-render.md
fn spec_gen(args: GenArgs) -> Result<()> {
    use openapi_codegen::{
        generate_for_target, GenOptions, HttpClient, Lang, TargetPolicy, MANIFEST_FILE,
    };

    const TARGET_POLICY: &str = include_str!("../../../../../clients/codegen.toml");

    let lang = match args.lang {
        GenLang::Ts => Lang::Ts,
        GenLang::Py => Lang::Py,
        GenLang::Rust => Lang::Rust,
    };
    let target = TargetPolicy::from_toml(TARGET_POLICY)?.resolve(lang, args.target.as_deref())?;
    let opts = GenOptions {
        lang,
        target: Some(target),
        spec_path: PathBuf::new(),
        out_dir: args.out.clone(),
        client_name: "createClient".to_string(),
        http_client: match args.http {
            GenHttp::Fetch => HttpClient::Fetch,
            GenHttp::Axios => HttpClient::Axios,
        },
        emit_types: true,
        emit_client: true,
        // TanStack Query hooks are a TypeScript-only concern.
        emit_hooks: matches!(lang, Lang::Ts),
    };
    let output = generate_for_target(&keep::spec::openapi_json(), &opts, target)?;
    output.write_to_dir(&args.out)?;
    for file in &output.files {
        let path = args.out.join(&file.rel_path);
        println!("generated {}", path.display());
    }
    println!("generated {}", args.out.join(MANIFEST_FILE).display());
    let requirements = output.requirements.expect("explicit target requirements");
    println!(
        "target: {} (minimum {} {})",
        requirements.target,
        requirements.language.id(),
        requirements.minimum_version
    );
    let entry_file = match lang {
        Lang::Ts => "index.ts",
        Lang::Py => "__init__.py",
        Lang::Rust => "mod.rs",
    };
    println!("next: {}", args.out.join(entry_file).display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cli, Command};
    use clap::Parser;

    /// `keep spec` / `keep spec gen` / `keep dockerfile render` parse with their
    /// convention flags (#777). Positionals name subcommands; format / lang /
    /// variant / version / out are flags.
    #[test]
    fn spec_verbs_parse() {
        // spec print variants.
        Cli::try_parse_from(["keep", "spec"]).expect("spec default");
        Cli::try_parse_from(["keep", "spec", "--format", "openapi-yaml"]).expect("spec yaml");
        Cli::try_parse_from(["keep", "spec", "--format", "json-schema"]).expect("spec json-schema");
        Cli::try_parse_from(["keep", "spec", "--shapes"]).expect("spec shapes");
        Cli::try_parse_from(["keep", "spec", "--fields"]).expect("spec fields");

        // spec gen.
        let cli = Cli::try_parse_from([
            "keep",
            "spec",
            "gen",
            "--lang",
            "ts",
            "--target",
            "typescript-5.0",
            "--out",
            "/tmp/x",
        ])
        .expect("spec gen should parse");
        match cli.cmd {
            Some(Command::Spec(SpecArgs {
                gen: Some(SpecSub::Gen(a)),
                ..
            })) => {
                assert!(matches!(a.lang, GenLang::Ts));
                assert_eq!(a.target.as_deref(), Some("typescript-5.0"));
                assert_eq!(a.out, PathBuf::from("/tmp/x"));
            }
            other => panic!("expected spec gen, got {other:?}"),
        }
    }
}
