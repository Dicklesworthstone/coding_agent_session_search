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

fn is_robot_mode_args() -> bool {
    for arg in std::env::args() {
        if matches!(arg.as_str(), "--json" | "--robot" | "-json" | "-robot") {
            return true;
        }
        if arg == "--robot-format" || arg.starts_with("--robot-format=") {
            return true;
        }
    }
    env_requests_robot_output()
}

fn handle_fatal_error(err: coding_agent_search::CliError) -> ! {
    if err.was_already_reported() {
        std::process::exit(err.code);
    }

    // If the message looks like JSON, output it directly (it's a pre-formatted robot error).
    // Also enforce JSON if robot mode flags were detected in raw args.
    if err.message.trim().starts_with('{') {
        eprintln!("{}", err.message);
    } else if is_robot_mode_args() {
        // Wrap unstructured error for robot
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
        // Human-readable output
        eprintln!("{}", err.message);
    }
    std::process::exit(err.code);
}

const DEFAULT_TANTIVY_MAX_WRITER_THREADS: usize = 26;

fn apply_default_tantivy_writer_thread_cap() {
    let configured = dotenvy::var("CASS_TANTIVY_MAX_WRITER_THREADS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0);
    if configured.is_none() {
        // Empirical full-corpus benchmarking on a 128-core host found that a
        // 26-thread Tantivy writer beats the previous 32-thread default by
        // reducing startup overhead and writer contention without hurting the
        // rebuild window.
        unsafe {
            std::env::set_var(
                "CASS_TANTIVY_MAX_WRITER_THREADS",
                DEFAULT_TANTIVY_MAX_WRITER_THREADS.to_string(),
            );
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
