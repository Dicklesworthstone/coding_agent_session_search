fn env_requests_robot_output() -> bool {
    let cass_output_format = dotenvy::var("CASS_OUTPUT_FORMAT")
        .ok()
        .map(|value| value.trim().to_ascii_lowercase())
        .is_some_and(|value| {
            matches!(
                value.as_str(),
                "json" | "jsonl" | "compact" | "sessions" | "toon"
            )
        });
    let toon_default_format = dotenvy::var("TOON_DEFAULT_FORMAT")
        .ok()
        .map(|value| value.trim().to_ascii_lowercase())
        .is_some_and(|value| matches!(value.as_str(), "json" | "toon"));
    cass_output_format || toon_default_format
}

fn is_robot_format_name(value: &str) -> bool {
    let value = value.trim().to_ascii_lowercase();
    matches!(
        value.as_str(),
        "json" | "jsonl" | "compact" | "sessions" | "toon"
    )
}

fn raw_command_name(args: &[String]) -> Option<&str> {
    let mut index = 1;
    while index < args.len() {
        let arg = args[index].as_str();

        if arg == "--" {
            return args.get(index + 1).map(String::as_str);
        }

        if matches!(
            arg,
            "--color"
                | "--progress"
                | "--wrap"
                | "--db"
                | "--trace-file"
                | "--data-dir"
                | "--stale-threshold"
                | "--robot-format"
                | "--format"
                | "--output"
                | "--output-format"
                | "--output_format"
        ) {
            index += 2;
            continue;
        }

        if arg.starts_with("--color=")
            || arg.starts_with("--progress=")
            || arg.starts_with("--wrap=")
            || arg.starts_with("--db=")
            || arg.starts_with("--trace-file=")
            || arg.starts_with("--data-dir=")
            || arg.starts_with("--stale-threshold=")
            || arg.starts_with("--robot-format=")
            || arg.starts_with("--format=")
            || arg.starts_with("--output=")
            || arg.starts_with("--output-format=")
            || arg.starts_with("--output_format=")
        {
            index += 1;
            continue;
        }

        if matches!(
            arg,
            "--json"
                | "--robot"
                | "-json"
                | "-robot"
                | "--nowrap"
                | "--quiet"
                | "-q"
                | "--verbose"
                | "-v"
                | "--robot-help"
        ) {
            index += 1;
            continue;
        }

        if arg.starts_with('-') {
            index += 1;
            continue;
        }

        return Some(arg);
    }
    None
}

fn is_robot_mode_args() -> bool {
    let args: Vec<String> = std::env::args().collect();
    let command_name = raw_command_name(&args);
    for (index, arg) in args.iter().enumerate() {
        if matches!(arg.as_str(), "--json" | "--robot" | "-json" | "-robot") {
            return true;
        }
        if arg == "--robot-format" || arg.starts_with("--robot-format=") {
            return true;
        }
        if let Some(value) = arg.strip_prefix("--format=")
            && is_robot_format_name(value)
            && command_name != Some("export")
        {
            return true;
        }
        if let Some(value) = arg
            .strip_prefix("--output=")
            .or_else(|| arg.strip_prefix("--output-format="))
            .or_else(|| arg.strip_prefix("--output_format="))
            && is_robot_format_name(value)
            && command_name != Some("export")
        {
            return true;
        }
        if arg == "--format"
            && args
                .get(index + 1)
                .is_some_and(|value| is_robot_format_name(value))
            && command_name != Some("export")
        {
            return true;
        }
        if matches!(
            arg.as_str(),
            "--output" | "--output-format" | "--output_format"
        ) && args
            .get(index + 1)
            .is_some_and(|value| is_robot_format_name(value))
            && command_name != Some("export")
        {
            return true;
        }
    }
    env_requests_robot_output()
}

fn handle_fatal_error(err: coding_agent_search::CliError) -> ! {
    if err.was_already_reported() {
        std::process::exit(err.code);
    }

    // Robot-mode success payloads use stdout; fatal diagnostics, including
    // structured error envelopes, stay on stderr so stdout remains data-only.
    if err.message.trim().starts_with('{') {
        // Pre-formatted JSON error envelope from a robot-mode subcommand.
        eprintln!("{}", err.message);
    } else if is_robot_mode_args() {
        // Wrap unstructured error for robot consumers.
        let payload = serde_json::json!({
            "error": {
                "code": err.code,
                "kind": err.kind,
                "message": err.message,
                "hint": err.hint,
                "retryable": err.retryable,
            }
        });
        eprintln!("{payload}");
    } else {
        // Human-readable output stays on stderr per Unix convention.
        eprintln!("{}", err.message);
    }
    std::process::exit(err.code);
}

fn apply_default_tantivy_writer_thread_cap() {
    let configured = dotenvy::var("CASS_TANTIVY_MAX_WRITER_THREADS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0);
    if configured.is_none() {
        // Keep explicit operator tuning authoritative, otherwise use the same
        // memory-aware default as the search layer before frankensearch opens
        // any Tantivy writers.
        let default_cap =
            coding_agent_search::search::tantivy::default_tantivy_max_writer_threads();
        unsafe {
            std::env::set_var("CASS_TANTIVY_MAX_WRITER_THREADS", default_cap.to_string());
        }
    }
}

fn main() -> anyhow::Result<()> {
    // Check for AVX support before anything else. ONNX Runtime requires AVX
    // instructions and will crash with SIGILL on CPUs that lack them.
    #[cfg(target_arch = "x86_64")]
    {
        if !std::arch::is_x86_feature_detected!("avx") {
            eprintln!(
                "Error: Your CPU does not support AVX instructions, which are required by cass.\n\
                 \n\
                 The ONNX Runtime dependency used for semantic search requires AVX support.\n\
                 AVX is available on most x86_64 CPUs manufactured from ~2011 onwards\n\
                 (Intel Sandy Bridge / AMD Bulldozer and later).\n\
                 \n\
                 Without AVX, the process would crash with a SIGILL (illegal instruction) signal.\n\
                 Please run cass on a machine with a newer CPU that supports AVX."
            );
            std::process::exit(1);
        }
    }

    // Load .env early; ignore if missing.
    dotenvy::dotenv().ok();
    apply_default_tantivy_writer_thread_cap();

    let raw_args: Vec<String> = std::env::args().collect();
    let parsed = match coding_agent_search::parse_cli(raw_args) {
        Ok(parsed) => parsed,
        Err(err) => handle_fatal_error(err),
    };

    let use_current_thread = matches!(
        parsed.cli.command,
        Some(coding_agent_search::Commands::Search { .. })
    );
    let runtime = if use_current_thread {
        asupersync::runtime::RuntimeBuilder::current_thread().build()?
    } else {
        asupersync::runtime::RuntimeBuilder::multi_thread().build()?
    };

    match runtime.block_on(coding_agent_search::run_with_parsed(parsed)) {
        Ok(()) => Ok(()),
        Err(err) => handle_fatal_error(err),
    }
}
