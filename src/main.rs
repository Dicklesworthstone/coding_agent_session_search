fn is_robot_mode_args() -> bool {
    std::env::args()
        .any(|arg| arg == "--json" || arg == "--robot" || arg == "-json" || arg == "-robot")
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
