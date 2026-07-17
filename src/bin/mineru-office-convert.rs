use office2pdf::config::{ConvertOptions, Format};
use std::io::{Read, Write};

const INPUT_CAP: usize = 32 * 1024 * 1024;

fn main() {
    let mut args = std::env::args_os();
    let _ = args.next();
    let format = match (args.next().as_deref(), args.next()) {
        (Some(value), None) => match value.to_str() {
            Some("docx") => Format::Docx,
            Some("pptx") => Format::Pptx,
            Some("xlsx") => Format::Xlsx,
            _ => fail("invalid format"),
        },
        _ => fail("usage: mineru-office-convert <docx|pptx|xlsx>"),
    };
    let mut input = Vec::new();
    if std::io::stdin()
        .take((INPUT_CAP as u64) + 1)
        .read_to_end(&mut input)
        .is_err()
        || input.len() > INPUT_CAP
    {
        fail("input too large");
    }
    let result = office2pdf::convert_bytes(&input, format, &ConvertOptions::default())
        .unwrap_or_else(|_| fail("conversion failed"));
    if !result.pdf.starts_with(b"%PDF-") {
        fail("conversion produced invalid PDF");
    }
    if !result.warnings.is_empty() {
        eprintln!("conversion warnings: {}", result.warnings.len());
    }
    if std::io::stdout().write_all(&result.pdf).is_err() {
        fail("output failed");
    }
}

fn fail(message: &str) -> ! {
    eprintln!("{message}");
    std::process::exit(1)
}
