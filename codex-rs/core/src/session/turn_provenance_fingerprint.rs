use super::stable_provenance_id;
use codex_protocol::models::ImageDetail;
use codex_protocol::user_input::UserInput;
use sha1::Digest;
use sha1::Sha1;
use std::io::Write;
use std::path::Path;
use uuid::Uuid;

const STRUCTURED_INPUT_VERSION: &[u8] = b"codex-structured-input-v1";

pub(super) fn request_fingerprint(user_input: &[UserInput]) -> String {
    let mut hasher = Sha1::new();
    hash_normalized_text(&mut hasher, user_input);
    if has_structured_input(user_input) {
        hash_frame(&mut hasher, b"structured-input", STRUCTURED_INPUT_VERSION);
        hash_u64(
            &mut hasher,
            b"structured-input-count",
            user_input.len() as u64,
        );
        for input in user_input {
            hash_structured_input(&mut hasher, input);
        }
    }
    let digest = hasher.finalize();
    stable_provenance_id("request", &format!("{digest:x}"))
}

fn hash_normalized_text(hasher: &mut Sha1, user_input: &[UserInput]) {
    let mut wrote_text = false;
    let mut pending_space = false;
    for input in user_input {
        let UserInput::Text { text, .. } = input else {
            continue;
        };
        if wrote_text {
            pending_space = true;
        }
        for character in text.chars() {
            if character.is_whitespace() {
                pending_space = true;
                continue;
            }
            if pending_space && wrote_text {
                hasher.update(b" ");
            }
            let mut encoded = [0; 4];
            hasher.update(character.encode_utf8(&mut encoded).as_bytes());
            wrote_text = true;
            pending_space = false;
        }
    }
}

fn has_structured_input(user_input: &[UserInput]) -> bool {
    user_input.iter().any(|input| match input {
        UserInput::Text { text_elements, .. } => !text_elements.is_empty(),
        UserInput::Image { .. }
        | UserInput::LocalImage { .. }
        | UserInput::Audio { .. }
        | UserInput::LocalAudio { .. }
        | UserInput::Skill { .. }
        | UserInput::Mention { .. } => true,
        _ => true,
    })
}

fn hash_structured_input(hasher: &mut Sha1, input: &UserInput) {
    match input {
        UserInput::Text { text_elements, .. } => {
            hash_tag(hasher, b"text");
            let mut text_hasher = Sha1::new();
            hash_normalized_text(&mut text_hasher, std::slice::from_ref(input));
            let text_digest = text_hasher.finalize();
            hash_frame(hasher, b"text-normalized", text_digest.as_slice());
            hash_u64(hasher, b"text-element-count", text_elements.len() as u64);
            for element in text_elements {
                hash_u64(
                    hasher,
                    b"text-element-start",
                    element.byte_range.start as u64,
                );
                hash_u64(hasher, b"text-element-end", element.byte_range.end as u64);
                match element._placeholder_for_conversion_only() {
                    Some(placeholder) => {
                        hash_frame(hasher, b"text-element-placeholder", placeholder.as_bytes())
                    }
                    None => hash_tag(hasher, b"text-element-placeholder-none"),
                }
            }
        }
        UserInput::Image { image_url, detail } => {
            hash_tag(hasher, b"image");
            hash_frame(hasher, b"image-url", image_url.as_bytes());
            hash_image_detail(hasher, *detail);
        }
        UserInput::LocalImage { path, detail } => {
            hash_tag(hasher, b"local-image");
            hash_path(hasher, b"local-image-path", path);
            hash_image_detail(hasher, *detail);
        }
        UserInput::Audio { audio_url } => {
            hash_tag(hasher, b"audio");
            hash_frame(hasher, b"audio-url", audio_url.as_bytes());
        }
        UserInput::LocalAudio { path } => {
            hash_tag(hasher, b"local-audio");
            hash_path(hasher, b"local-audio-path", path);
        }
        UserInput::Skill { name, path } => {
            hash_tag(hasher, b"skill");
            hash_frame(hasher, b"skill-name", name.as_bytes());
            hash_path(hasher, b"skill-path", path);
        }
        UserInput::Mention { name, path } => {
            hash_tag(hasher, b"mention");
            hash_frame(hasher, b"mention-name", name.as_bytes());
            hash_frame(hasher, b"mention-path", path.as_bytes());
        }
        _ => {
            hash_tag(hasher, b"unknown-user-input");
            let mut writer = Sha1Writer(hasher);
            if serde_json::to_writer(&mut writer, input).is_err() {
                // Identity is incomplete: prefer a duplicate advisory over merging unrelated inputs.
                let salt = Uuid::new_v4();
                hash_frame(
                    hasher,
                    b"unknown-user-input-serialization-failed",
                    salt.as_bytes(),
                );
            }
        }
    }
}

fn hash_image_detail(hasher: &mut Sha1, detail: Option<ImageDetail>) {
    let label = match detail {
        None => b"none".as_slice(),
        Some(ImageDetail::Auto) => b"auto",
        Some(ImageDetail::Low) => b"low",
        Some(ImageDetail::High) => b"high",
        Some(ImageDetail::Original) => b"original",
    };
    hash_frame(hasher, b"image-detail", label);
}

fn hash_path(hasher: &mut Sha1, field: &[u8], path: &Path) {
    hash_frame(hasher, field, path.as_os_str().as_encoded_bytes());
}

fn hash_tag(hasher: &mut Sha1, tag: &[u8]) {
    hash_frame(hasher, tag, &[]);
}

fn hash_u64(hasher: &mut Sha1, field: &[u8], value: u64) {
    hash_frame(hasher, field, &value.to_be_bytes());
}

fn hash_frame(hasher: &mut Sha1, field: &[u8], value: &[u8]) {
    hasher.update((field.len() as u64).to_be_bytes());
    hasher.update(field);
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

struct Sha1Writer<'a>(&'a mut Sha1);

impl Write for Sha1Writer<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
