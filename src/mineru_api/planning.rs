use super::{Backend, InputDocument, PlannedTask};
use std::path::Path;

#[doc(hidden)]
pub fn selected_pages_for_path(
    path: &Path,
    is_pdf: bool,
    start: u64,
    end: Option<u64>,
) -> Result<usize, String> {
    if !is_pdf {
        return Ok(1);
    }
    let document = lopdf::Document::load(path).map_err(|_| "cannot read PDF")?;
    if document.is_encrypted() {
        return Err("PDF is encrypted".into());
    }
    selected_pages(document.get_pages().len(), start, end)
}
pub(crate) fn selected_pages(count: usize, start: u64, end: Option<u64>) -> Result<usize, String> {
    let count = u64::try_from(count).map_err(|_| "PDF page count is too large")?;
    let last = count.checked_sub(1).ok_or("PDF has no pages")?;
    let end = end.unwrap_or(last).min(last);
    if start > end {
        return Err("PDF page range is invalid".into());
    }
    usize::try_from(
        end.checked_sub(start)
            .and_then(|pages| pages.checked_add(1))
            .ok_or("PDF page range is invalid")?,
    )
    .map_err(|_| "PDF page range is too large".into())
}

fn truncate_utf8(value: &str, max: usize) -> String {
    value
        .char_indices()
        .take_while(|&(i, c)| i + c.len_utf8() <= max)
        .map(|(_, c)| c)
        .collect()
}
fn candidate(stem: &str, suffix: &str) -> String {
    if stem.len() + suffix.len() <= 200 {
        format!("{stem}{suffix}")
    } else if suffix.len() >= 200 {
        truncate_utf8(suffix, 200)
    } else {
        format!("{}{suffix}", truncate_utf8(stem, 200 - suffix.len()))
    }
}
#[cfg(feature = "internal-mineru-api-client")]
fn fold_key(value: &str) -> String {
    caseless::default_case_fold_str(value)
}
/// P2D gate: this std-only lowercase key differs from Python's full Unicode casefold.
#[cfg(not(feature = "internal-mineru-api-client"))]
fn fold_key(value: &str) -> String {
    value.to_lowercase()
}
pub fn unique_stems(stems: &[String]) -> Vec<String> {
    let normalized: Vec<_> = stems.iter().map(|s| truncate_utf8(s, 200)).collect();
    let raw: std::collections::HashSet<_> = normalized.iter().map(|s| fold_key(s)).collect();
    let mut counts = std::collections::HashMap::<String, usize>::new();
    let mut assigned = std::collections::HashSet::new();
    let mut out = Vec::new();
    for (original, normalized) in stems.iter().zip(normalized) {
        let base = if normalized.is_empty() {
            original.clone()
        } else {
            normalized
        };
        let k = fold_key(&base);
        let count = counts.entry(k.clone()).or_default();
        let value = if *count == 0 && !assigned.contains(&k) {
            base
        } else {
            let mut n = *count + 1;
            loop {
                let c = candidate(&base, &format!("_{n}"));
                let ck = fold_key(&c);
                if !raw.contains(&ck) && !assigned.contains(&ck) {
                    break c;
                }
                n += 1;
            }
        };
        *count += 1;
        assigned.insert(fold_key(&value));
        out.push(value);
    }
    out
}

pub(crate) fn plan_tasks(
    backend: Backend,
    documents: &[InputDocument],
    window: usize,
) -> Result<Vec<PlannedTask>, String> {
    if window == 0 || documents.iter().any(|d| d.effective_pages == 0) {
        return Err("pages and processing window must be nonzero".into());
    }
    if backend != Backend::Pipeline {
        return Ok(documents
            .iter()
            .cloned()
            .enumerate()
            .map(|(index, d)| PlannedTask {
                index: index + 1,
                total_pages: d.effective_pages,
                documents: vec![d],
            })
            .collect());
    }
    let mut order: Vec<_> = documents.iter().cloned().collect();
    order.sort_by_key(|d| (std::cmp::Reverse(d.effective_pages), d.order));
    let mut bins: Vec<PlannedTask> = Vec::new();
    for doc in order {
        if doc.effective_pages > window {
            bins.push(PlannedTask {
                index: 0,
                total_pages: doc.effective_pages,
                documents: vec![doc],
            });
            continue;
        }
        let target = bins
            .iter()
            .enumerate()
            .filter(|(_, b)| {
                b.total_pages
                    .checked_add(doc.effective_pages)
                    .is_some_and(|total| total <= window)
            })
            .min_by_key(|(i, b)| (b.total_pages, *i))
            .map(|(i, _)| i);
        match target.and_then(|i| {
            bins[i]
                .total_pages
                .checked_add(doc.effective_pages)
                .map(|total| (i, total))
        }) {
            Some((i, total)) => {
                bins[i].total_pages = total;
                bins[i].documents.push(doc);
            }
            None => bins.push(PlannedTask {
                index: 0,
                total_pages: doc.effective_pages,
                documents: vec![doc],
            }),
        }
    }
    for (index, task) in bins.iter_mut().enumerate() {
        task.index = index + 1;
    }
    Ok(bins)
}
pub(crate) fn effective_concurrency(
    local: usize,
    server: usize,
    tasks: usize,
) -> Result<usize, String> {
    if local == 0 || server == 0 || tasks == 0 {
        Err("concurrency operands must be positive".into())
    } else {
        Ok(local.min(server).min(tasks))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    fn d(order: usize, pages: usize) -> InputDocument {
        InputDocument {
            path: PathBuf::from(format!("{order}.pdf")),
            suffix: "pdf".into(),
            stem: format!("s{order}"),
            effective_pages: pages,
            order,
        }
    }
    #[test]
    fn stems_reserve_raw_names_and_utf8_bytes() {
        let input = vec!["a".into(), "A".into(), "a_2".into(), "a".into()];
        assert_eq!(unique_stems(&input), vec!["a", "A_3", "a_2", "a_4"]);
        let long = "é".repeat(101);
        assert_eq!(unique_stems(&[long])[0].len(), 200);
        #[cfg(not(feature = "internal-mineru-api-client"))]
        assert_eq!(unique_stems(&["ß".into(), "ss".into()]), vec!["ß", "ss"]); // Known std lowercase gap.
        #[cfg(feature = "internal-mineru-api-client")]
        assert_eq!(
            unique_stems(&["Straße".into(), "STRASSE".into()]),
            vec!["Straße", "STRASSE_2"]
        );
        #[cfg(feature = "internal-mineru-api-client")]
        assert_eq!(fold_key("İ"), "i\u{307}");
        #[cfg(feature = "internal-mineru-api-client")]
        assert_eq!(
            unique_stems(&["İ".into(), "i\u{307}".into(), "i".into()]),
            vec!["İ", "i\u{307}_2", "i"]
        );
        #[cfg(feature = "internal-mineru-api-client")]
        assert_eq!(fold_key("Σ"), fold_key("σ"));
        #[cfg(feature = "internal-mineru-api-client")]
        assert_eq!(fold_key("σ"), fold_key("ς"));
        #[cfg(feature = "internal-mineru-api-client")]
        assert_eq!(
            unique_stems(&["Σ".into(), "σ_2".into(), "σ".into(), "ς".into()]),
            vec!["Σ", "σ_2", "σ_3", "ς_4"]
        );
        #[cfg(feature = "internal-mineru-api-client")]
        assert_eq!(
            unique_stems(&["a".into(), "A_2".into(), "A".into()]),
            vec!["a", "A_2", "A_3"]
        );
    }
    #[test]
    fn pipeline_plans_and_other_backends_do_not_pack() {
        let docs = vec![d(5, 70), d(2, 40), d(4, 24), d(1, 24), d(3, 16)];
        let plan = plan_tasks(Backend::Pipeline, &docs, 64).unwrap();
        assert_eq!(
            plan.iter()
                .map(|t| (
                    t.index,
                    t.total_pages,
                    t.documents
                        .iter()
                        .map(|d| d.stem.as_str())
                        .collect::<Vec<_>>()
                ))
                .collect::<Vec<_>>(),
            vec![
                (1, 70, vec!["s5"]),
                (2, 64, vec!["s2", "s1"]),
                (3, 40, vec!["s4", "s3"])
            ]
        );
        for backend in [
            Backend::VlmEngine,
            Backend::VlmHttpClient,
            Backend::HybridEngine,
            Backend::HybridHttpClient,
        ] {
            let tasks = plan_tasks(backend, &docs, 64).unwrap();
            assert_eq!(
                tasks
                    .iter()
                    .map(|t| (t.index, t.documents[0].stem.as_str()))
                    .collect::<Vec<_>>(),
                vec![(1, "s5"), (2, "s2"), (3, "s4"), (4, "s1"), (5, "s3")]
            );
        }
        assert!(plan_tasks(Backend::Pipeline, &docs, 0).is_err());
        assert!(plan_tasks(Backend::Pipeline, &[d(0, 0)], 64).is_err());
        assert_eq!(effective_concurrency(3, 2, 9), Ok(2));
        assert!(effective_concurrency(1, 1, 0).is_err());
    }
    #[test]
    fn pipeline_planning_overflow_starts_a_new_bin() {
        let plan = plan_tasks(Backend::Pipeline, &[d(0, usize::MAX), d(1, 1)], usize::MAX).unwrap();

        assert_eq!(
            plan.iter()
                .map(|task| (task.index, task.total_pages))
                .collect::<Vec<_>>(),
            vec![(1, usize::MAX), (2, 1)]
        );
    }
}
