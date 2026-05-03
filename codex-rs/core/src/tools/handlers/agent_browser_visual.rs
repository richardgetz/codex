use std::fs;
use std::path::Path;
use std::process::Stdio;

use image::ExtendedColorType;
use image::ImageBuffer;
use image::ImageEncoder;
use image::Rgba;
use image::codecs::png::CompressionType;
use image::codecs::png::FilterType;
use image::codecs::png::PngEncoder;
use serde_json::Value;

use crate::function_tool::FunctionCallError;

pub(crate) fn write_visual_shell(
    path: &Path,
    snapshot: &Value,
    viewport_width: u32,
    viewport_height: u32,
) -> Result<(), FunctionCallError> {
    fs::write(
        path,
        visual_shell_html(snapshot, viewport_width, viewport_height),
    )
    .map_err(|err| {
        FunctionCallError::RespondToModel(format!(
            "failed to write Obscura headful mirror `{}`: {err}",
            path.display()
        ))
    })
}

pub(crate) fn open_visual_shell(path: &Path) -> bool {
    if std::env::var("CODEX_AGENT_BROWSER_DISABLE_OPEN").as_deref() == Ok("1") {
        return false;
    }

    let mut command = if cfg!(target_os = "macos") {
        let mut command = std::process::Command::new("open");
        command.arg(path);
        command
    } else if cfg!(target_os = "windows") {
        let mut command = std::process::Command::new("cmd");
        command.arg("/C").arg("start").arg("").arg(path);
        command
    } else {
        let mut command = std::process::Command::new("xdg-open");
        command.arg(path);
        command
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .is_ok()
}

pub(crate) fn visual_shell_html(
    snapshot: &Value,
    viewport_width: u32,
    viewport_height: u32,
) -> String {
    let title = html_escape(snapshot.get("title").and_then(Value::as_str).unwrap_or(""));
    let url = html_escape(snapshot.get("url").and_then(Value::as_str).unwrap_or(""));
    let text = html_escape(snapshot.get("text").and_then(Value::as_str).unwrap_or(""));
    let width = viewport_width.clamp(/*min*/ 320, /*max*/ 1600);
    let height = viewport_height.clamp(/*min*/ 240, /*max*/ 1200);
    let mut element_markup = String::new();
    if let Some(elements) = snapshot.get("elements").and_then(Value::as_array) {
        for element in elements.iter().take(160) {
            let label = html_escape(element.get("label").and_then(Value::as_str).unwrap_or(""));
            let tag = html_escape(element.get("tag").and_then(Value::as_str).unwrap_or("el"));
            let ref_id = html_escape(element.get("ref").and_then(Value::as_str).unwrap_or(""));
            let Some(rect) = element.get("rect") else {
                continue;
            };
            let x = rect.get("x").and_then(Value::as_i64).unwrap_or(0).max(0);
            let y = rect.get("y").and_then(Value::as_i64).unwrap_or(0).max(0);
            let w = rect
                .get("width")
                .and_then(Value::as_i64)
                .unwrap_or(1)
                .max(1);
            let h = rect
                .get("height")
                .and_then(Value::as_i64)
                .unwrap_or(1)
                .max(1);
            element_markup.push_str(&format!(
                r#"<div class="target" style="left:{x}px;top:{y}px;width:{w}px;height:{h}px"><span>{tag} {ref_id} {label}</span></div>"#
            ));
        }
    }

    format!(
        r#"<!doctype html>
<meta charset="utf-8">
<title>Codex Obscura Mirror</title>
<style>
body{{margin:0;background:#f7f8fa;color:#111;font:13px -apple-system,BlinkMacSystemFont,Segoe UI,sans-serif}}
header{{height:48px;padding:8px 12px;box-sizing:border-box;background:#111;color:white}}
h1{{font-size:15px;line-height:18px;margin:0 0 3px}}
.url{{opacity:.75;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}}
.viewport{{position:relative;width:{width}px;height:{height}px;overflow:hidden;background:white;border-bottom:1px solid #d0d7de}}
.text{{position:absolute;inset:12px;white-space:pre-wrap;line-height:1.35;color:#24292f}}
.target{{position:absolute;border:2px solid #0b57d0;background:rgba(11,87,208,.08);box-sizing:border-box;pointer-events:none}}
.target span{{position:absolute;left:-2px;top:-22px;max-width:360px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis;background:#0b57d0;color:white;padding:2px 5px;font-size:11px}}
aside{{padding:12px;max-width:{width}px;box-sizing:border-box}}
pre{{white-space:pre-wrap;line-height:1.35;margin:0}}
</style>
<header><h1>Obscura headful mirror: {title}</h1><div class="url">{url}</div></header>
<main class="viewport"><pre class="text">{text}</pre>{element_markup}</main>
<aside><strong>Snapshot text</strong><pre>{text}</pre></aside>"#
    )
}

pub(crate) fn render_snapshot_png(
    snapshot: &Value,
    viewport_width: u32,
    viewport_height: u32,
    full_page: bool,
) -> Result<Vec<u8>, FunctionCallError> {
    let width = viewport_width.clamp(/*min*/ 320, /*max*/ 1600);
    let height = if full_page {
        viewport_height.clamp(/*min*/ 480, /*max*/ 2400)
    } else {
        viewport_height.clamp(/*min*/ 240, /*max*/ 1200)
    };
    let mut image = ImageBuffer::from_pixel(width, height, Rgba([248, 249, 250, 255]));
    draw_rect(
        &mut image,
        /*x*/ 0,
        /*y*/ 0,
        width,
        /*height*/ 44,
        Rgba([32, 33, 36, 255]),
    );
    draw_text_bar(
        &mut image,
        /*x*/ 10,
        /*y*/ 10,
        width / 3,
        Rgba([255, 255, 255, 255]),
    );

    let title = snapshot.get("title").and_then(Value::as_str).unwrap_or("");
    let url = snapshot.get("url").and_then(Value::as_str).unwrap_or("");
    draw_text_bar(
        &mut image,
        /*x*/ 10,
        /*y*/ 30,
        visual_bar_width(
            if title.is_empty() { url } else { title },
            width.saturating_sub(20),
        ),
        Rgba([218, 220, 224, 255]),
    );

    draw_rect(
        &mut image,
        /*x*/ 0,
        /*y*/ 44,
        width,
        height.saturating_sub(44),
        Rgba([255, 255, 255, 255]),
    );
    let text = snapshot.get("text").and_then(Value::as_str).unwrap_or("");
    let max_chars = usize::try_from(width / 7)
        .unwrap_or(80)
        .clamp(/*min*/ 24, /*max*/ 180);
    for (line_index, line) in wrap_visual_text(text, max_chars, if full_page { 90 } else { 42 })
        .iter()
        .enumerate()
    {
        let y = 58 + u32::try_from(line_index).unwrap_or(0) * 14;
        if y + 10 >= height {
            break;
        }
        draw_text_bar(
            &mut image,
            /*x*/ 12,
            y,
            visual_bar_width(line, width.saturating_sub(24)),
            Rgba([95, 99, 104, 255]),
        );
    }

    let viewport = snapshot.get("viewport");
    let source_width = viewport
        .and_then(|value| value.get("width"))
        .and_then(Value::as_f64)
        .unwrap_or(f64::from(viewport_width))
        .max(1.0);
    let source_height = viewport
        .and_then(|value| value.get("height"))
        .and_then(Value::as_f64)
        .unwrap_or(f64::from(viewport_height))
        .max(1.0);
    let scale_x = f64::from(width) / source_width;
    let scale_y = f64::from(height.saturating_sub(44)) / source_height;
    if let Some(elements) = snapshot.get("elements").and_then(Value::as_array) {
        for element in elements.iter().take(80) {
            let Some(rect) = element.get("rect") else {
                continue;
            };
            let x = rect
                .get("x")
                .and_then(Value::as_f64)
                .unwrap_or(0.0)
                .max(0.0)
                * scale_x;
            let y = 44.0
                + rect
                    .get("y")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0)
                    .max(0.0)
                    * scale_y;
            let w = rect
                .get("width")
                .and_then(Value::as_f64)
                .unwrap_or(1.0)
                .max(1.0)
                * scale_x;
            let h = rect
                .get("height")
                .and_then(Value::as_f64)
                .unwrap_or(1.0)
                .max(1.0)
                * scale_y;
            draw_outline(
                &mut image,
                /*x*/ x.round() as u32,
                /*y*/ y.round() as u32,
                w.round().max(1.0) as u32,
                /*height*/ h.round().max(1.0) as u32,
                Rgba([11, 87, 208, 255]),
            );
            let label = element
                .get("label")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .or_else(|| element.get("tag").and_then(Value::as_str))
                .unwrap_or("element");
            let label = compact_visual_text(label);
            if y >= 12.0 {
                draw_text_bar(
                    &mut image,
                    x.round() as u32,
                    (y - 10.0).round() as u32,
                    visual_bar_width(&label, /*max_width*/ 220),
                    Rgba([11, 87, 208, 255]),
                );
            }
        }
    }

    let mut encoded = Vec::new();
    let encoder =
        PngEncoder::new_with_quality(&mut encoded, CompressionType::Fast, FilterType::Sub);
    encoder
        .write_image(image.as_raw(), width, height, ExtendedColorType::Rgba8)
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "failed to encode Obscura DOM screenshot: {err}"
            ))
        })?;
    Ok(encoded)
}

fn html_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

fn wrap_visual_text(text: &str, max_chars: usize, max_lines: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for raw_line in text.lines() {
        let mut line = String::new();
        for word in raw_line.split_whitespace() {
            let next_len =
                line.chars().count() + word.chars().count() + usize::from(!line.is_empty());
            if next_len > max_chars && !line.is_empty() {
                lines.push(compact_visual_text(&line));
                line.clear();
                if lines.len() >= max_lines {
                    return lines;
                }
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        if !line.is_empty() {
            lines.push(compact_visual_text(&line));
        }
        if lines.len() >= max_lines {
            lines.truncate(max_lines);
            return lines;
        }
    }
    if lines.is_empty() {
        lines.push("(blank page)".to_string());
    }
    lines
}

fn compact_visual_text(text: &str) -> String {
    text.chars()
        .map(|ch| {
            if ch.is_ascii() && !ch.is_control() {
                ch
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn draw_rect(
    image: &mut ImageBuffer<Rgba<u8>, Vec<u8>>,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    color: Rgba<u8>,
) {
    let max_x = x.saturating_add(width).min(image.width());
    let max_y = y.saturating_add(height).min(image.height());
    for yy in y.min(image.height())..max_y {
        for xx in x.min(image.width())..max_x {
            image.put_pixel(xx, yy, color);
        }
    }
}

fn draw_outline(
    image: &mut ImageBuffer<Rgba<u8>, Vec<u8>>,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    color: Rgba<u8>,
) {
    draw_rect(image, x, y, width, /*height*/ 2, color);
    draw_rect(
        image,
        x,
        y.saturating_add(height.saturating_sub(2)),
        width,
        /*height*/ 2,
        color,
    );
    draw_rect(image, x, y, /*width*/ 2, height, color);
    draw_rect(
        image,
        x.saturating_add(width.saturating_sub(2)),
        y,
        /*width*/ 2,
        height,
        color,
    );
}

fn visual_bar_width(text: &str, max_width: u32) -> u32 {
    let width = u32::try_from(compact_visual_text(text).chars().count())
        .unwrap_or(max_width)
        .saturating_mul(6)
        .clamp(/*min*/ 18, /*max*/ max_width.max(18));
    width.min(max_width)
}

fn draw_text_bar(
    image: &mut ImageBuffer<Rgba<u8>, Vec<u8>>,
    x: u32,
    y: u32,
    width: u32,
    color: Rgba<u8>,
) {
    draw_rect(image, x, y, width.max(8), /*height*/ 4, color);
}
