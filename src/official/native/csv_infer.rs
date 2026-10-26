pub(super) fn infer_header(rows: &[Vec<String>]) -> bool {
    if rows.len() < 2 || rows[0].len() != modal_width(&rows[1..rows.len().min(51)]) {
        return false;
    }
    let header = &rows[0];
    let mut labels = std::collections::HashSet::new();
    if header.iter().enumerate().any(|(i, value)| {
        let label = stripped(value);
        let duplicate = !label.is_empty() && !labels.insert(unicase::UniCase::new(label));
        (label.is_empty() && i != 0)
            || value.chars().count() > 64
            || value.contains(['\n', '\r'])
            || duplicate
    }) {
        return false;
    }
    let mut header_votes = 0;
    let mut data_votes = 0;
    for (column, label) in header.iter().enumerate() {
        let values: Vec<_> = rows[1..rows.len().min(51)]
            .iter()
            .filter_map(|row| row.get(column))
            .map(|s| stripped(s))
            .filter(|s| !s.is_empty())
            .collect();
        if values.is_empty() {
            continue;
        }
        let kinds: Vec<_> = values.iter().map(|value| value_kind(value)).collect();
        let dominant = ["number", "boolean", "date", "text"]
            .into_iter()
            .find(|kind| {
                kinds
                    .iter()
                    .filter(|candidate| **candidate == *kind)
                    .count()
                    * 10
                    >= kinds.len() * 9
            });
        if dominant.is_some_and(|kind| kind != "text") {
            if value_kind(label) == "text" || (column == 0 && stripped(label).is_empty()) {
                header_votes += 1;
            } else {
                data_votes += 1;
            }
        } else if !stripped(label).is_empty()
            && values.iter().any(|v| unicase::eq(*v, stripped(label)))
        {
            data_votes += 1;
        }
    }
    if header_votes + data_votes > 0 {
        header_votes > data_votes
    } else {
        true
    }
}

fn stripped(value: &str) -> &str {
    value.trim_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
}

fn modal_width(rows: &[Vec<String>]) -> usize {
    let mut counts = std::collections::BTreeMap::new();
    for row in rows {
        *counts.entry(row.len()).or_insert(0usize) += 1;
    }
    counts
        .into_iter()
        .max_by_key(|(width, count)| (*count, *width))
        .map_or(0, |(width, _)| width)
}

fn value_kind(value: &str) -> &'static str {
    let value = stripped(value);
    if value.is_empty() {
        return "empty";
    }
    let numeric = value
        .strip_suffix('%')
        .map(stripped)
        .unwrap_or(value)
        .replace([',', ' ', '_', '\u{a0}'], "");
    static FLOAT_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let float = FLOAT_RE.get_or_init(|| {
        regex::Regex::new(r"^[+-]?(?:\d+(?:\.\d*)?|\.\d+)(?:[eE][+-]?\d+)?$").unwrap()
    });
    if numeric.chars().any(|c| c.is_ascii_digit()) && float.is_match(&numeric) {
        return "number";
    }
    if ["true", "false", "yes", "no"]
        .iter()
        .any(|candidate| unicase::eq(value, candidate))
    {
        return "boolean";
    }
    static DATE_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static TIME_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let date = DATE_RE.get_or_init(|| {
        regex::Regex::new(
            r"^\d{1,4}[-/.]\d{1,2}[-/.]\d{1,4}(?:[ T]\d{1,2}:\d{2}(?::\d{2})?(?:\.\d+)?Z?)?$",
        )
        .unwrap()
    });
    let time = TIME_RE
        .get_or_init(|| regex::Regex::new(r"^\d{1,2}:\d{2}(?::\d{2})?(?:\.\d+)?Z?$").unwrap());
    if date.is_match(value) || time.is_match(value) {
        return "date";
    }
    "text"
}
