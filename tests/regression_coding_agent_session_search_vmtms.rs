use coding_agent_search::ui::time_parser::parse_time_input;

#[test]
fn oversized_relative_time_filters_are_rejected_without_panicking() {
    let overflowing_inputs = [
        "9223372036854775807d",
        "-9223372036854775807d",
        "9223372036854775807 days ago",
        "9223372036854775807d ago",
    ];

    for input in overflowing_inputs {
        assert_eq!(parse_time_input(input), None, "input: {input}");
    }
}
