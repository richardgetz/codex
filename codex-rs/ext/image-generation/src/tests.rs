use codex_api::ImageBackground;
use codex_api::ImageEditRequest;
use codex_api::ImageGenerationRequest;
use codex_api::ImageQuality;
use codex_api::ImageUrl;
use codex_core::context::extension_image_generation_output_hint;
use codex_extension_api::ToolOutput;
use codex_extension_api::ToolPayload;
use codex_extension_api::ToolSpec;
use codex_protocol::models::ContentItem;
use codex_protocol::models::DEFAULT_IMAGE_DETAIL;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;
use codex_tools::ResponsesApiNamespaceTool;
use pretty_assertions::assert_eq;

use super::GeneratedImageOutput;
use super::ImageRequest;
use super::ImagegenArgs;
use super::imagegen_tool_spec;
use super::request_for_call_args;
use crate::IMAGE_GEN_NAMESPACE;
use crate::IMAGEGEN_TOOL_NAME;

const RESULT: &str = "cG5n";

#[test]
fn uses_reserved_image_gen_namespace() {
    let ToolSpec::Namespace(spec) = imagegen_tool_spec() else {
        panic!("imagegen should advertise a namespace tool");
    };
    assert_eq!(spec.name, IMAGE_GEN_NAMESPACE);
    let ResponsesApiNamespaceTool::Function(function) = &spec.tools[0];
    assert_eq!(function.name, IMAGEGEN_TOOL_NAME);
}

#[tokio::test]
async fn omitted_references_generate_with_fixed_defaults() {
    assert_eq!(
        request_for_call_args(
            &ImagegenArgs {
                prompt: "paint a moonlit lake".to_string(),
                referenced_image_paths: None,
                num_last_images_to_include: None,
            },
            &[],
            &[],
        )
        .await
        .expect("generation request should build"),
        ImageRequest::Generate(ImageGenerationRequest {
            prompt: "paint a moonlit lake".to_string(),
            background: Some(ImageBackground::Auto),
            model: "gpt-image-2".to_string(),
            n: None,
            quality: Some(ImageQuality::Auto),
            size: Some("auto".to_string()),
        })
    );
}

#[tokio::test]
async fn recent_image_fallback_preserves_latest_user_anchor_and_generated_context() {
    let history = vec![
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![
                input_image("user-1"),
                input_image("user-2"),
                ContentItem::InputText {
                    text: "edit these".to_string(),
                },
            ],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::FunctionCall {
            id: None,
            name: "mcp_image".to_string(),
            namespace: None,
            arguments: "{}".to_string(),
            call_id: "mcp-call".to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::FunctionCallOutput {
            id: None,
            call_id: "mcp-call".to_string(),
            output: image_output("mcp"),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::CustomToolCall {
            id: None,
            status: Some("completed".to_string()),
            call_id: "code-mode-call".to_string(),
            name: "exec".to_string(),
            namespace: None,
            input: String::new(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::CustomToolCallOutput {
            id: None,
            call_id: "code-mode-call".to_string(),
            name: Some("exec".to_string()),
            output: image_output("code-mode"),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::ImageGenerationCall {
            id: Some("generated-call".to_string()),
            status: "completed".to_string(),
            revised_prompt: None,
            result: "generated".to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::FunctionCallOutput {
            id: None,
            call_id: "orphan-call".to_string(),
            output: image_output("orphan"),
            internal_chat_message_metadata_passthrough: None,
        },
    ];

    assert_eq!(
        request_for_call_args(
            &ImagegenArgs {
                prompt: "change the lighting".to_string(),
                referenced_image_paths: None,
                num_last_images_to_include: Some(4),
            },
            &history,
            &[],
        )
        .await
        .expect("history-backed edit request should build"),
        ImageRequest::Edit(expected_edit_request(
            "change the lighting",
            &["user-1", "user-2", "code-mode", "generated"],
        ))
    );
}

#[tokio::test]
async fn conflicting_image_selectors_return_tool_error() {
    let error = request_for_call_args(
        &ImagegenArgs {
            prompt: "change the lighting".to_string(),
            referenced_image_paths: Some(vec![
                "/tmp/image.png"
                    .try_into()
                    .expect("test path should be absolute"),
            ]),
            num_last_images_to_include: Some(1),
        },
        &[],
        &[],
    )
    .await
    .expect_err("conflicting selectors should fail");

    assert_eq!(
        error.to_string(),
        "provide only one of `referenced_image_paths` or `num_last_images_to_include`"
    );
}

#[tokio::test]
async fn too_many_referenced_image_paths_return_tool_error() {
    let error = request_for_call_args(
        &ImagegenArgs {
            prompt: "change the lighting".to_string(),
            referenced_image_paths: Some(
                (0..6)
                    .map(|index| {
                        format!("/tmp/image-{index}.png")
                            .try_into()
                            .expect("test path should be absolute")
                    })
                    .collect(),
            ),
            num_last_images_to_include: None,
        },
        &[],
        &[],
    )
    .await
    .expect_err("too many paths should fail before reading files");

    assert_eq!(
        error.to_string(),
        "`referenced_image_paths` must contain at most 5 paths"
    );
}

#[tokio::test]
async fn recent_image_fallback_requires_requested_count() {
    let error = request_for_call_args(
        &ImagegenArgs {
            prompt: "change the lighting".to_string(),
            referenced_image_paths: None,
            num_last_images_to_include: Some(2),
        },
        &[ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![input_image("only-image")],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }],
        &[],
    )
    .await
    .expect_err("history-backed edit should require the requested image count");

    assert_eq!(
        error.to_string(),
        "requested the last 2 conversation images, but only 1 were available"
    );
}

#[test]
fn generated_output_returns_image_input_and_output_hint() {
    let output_hint =
        extension_image_generation_output_hint("/tmp", "/tmp/call-1.png").expect("hint should fit");
    let output = GeneratedImageOutput {
        result: RESULT.to_string(),
        output_hint: Some(output_hint.clone()),
    };

    let ResponseInputItem::FunctionCallOutput {
        output: response_output,
        ..
    } = output.to_response_item("call-1", &function_payload())
    else {
        panic!("imagegen should return function tool output");
    };
    let FunctionCallOutputBody::ContentItems(content_items) = response_output.body else {
        panic!("imagegen output should contain generated image bytes");
    };
    assert_eq!(
        content_items,
        vec![
            FunctionCallOutputContentItem::InputImage {
                image_url: format!("data:image/png;base64,{RESULT}"),
                detail: Some(DEFAULT_IMAGE_DETAIL),
            },
            FunctionCallOutputContentItem::InputText { text: output_hint },
        ]
    );
}

#[test]
fn generated_output_returns_generated_image_helper_input_in_code_mode() {
    let output = GeneratedImageOutput {
        result: RESULT.to_string(),
        output_hint: Some("generated image save hint".to_string()),
    };

    assert_eq!(
        output.code_mode_result(&function_payload()),
        serde_json::json!({
            "image_url": format!("data:image/png;base64,{RESULT}"),
            "output_hint": "generated image save hint",
        })
    );
}

#[test]
fn generated_output_omits_oversized_output_hint() {
    let long_path = "x".repeat(1024);
    let output = GeneratedImageOutput {
        result: RESULT.to_string(),
        output_hint: extension_image_generation_output_hint("/tmp", long_path),
    };

    let ResponseInputItem::FunctionCallOutput {
        output: response_output,
        ..
    } = output.to_response_item("call-1", &function_payload())
    else {
        panic!("imagegen should return function tool output");
    };
    let FunctionCallOutputBody::ContentItems(content_items) = response_output.body else {
        panic!("imagegen output should contain generated image bytes");
    };
    assert_eq!(
        content_items,
        vec![FunctionCallOutputContentItem::InputImage {
            image_url: format!("data:image/png;base64,{RESULT}"),
            detail: Some(DEFAULT_IMAGE_DETAIL),
        }]
    );
}

#[tokio::test]
async fn edit_matches_context_selector_for_generated_images_after_latest_user_anchor() {
    let history = vec![
        generated_item("g1"),
        generated_item("g2"),
        generated_item("g3"),
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![
                ContentItem::InputImage {
                    image_url: "data:image/png;base64,u1".to_string(),
                    detail: None,
                },
                ContentItem::InputImage {
                    image_url: "data:image/png;base64,u2".to_string(),
                    detail: None,
                },
            ],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        generated_item("g4"),
        generated_item("g5"),
        generated_item("g6"),
        generated_item("g7"),
    ];

    assert_eq!(
        edit_request("change the lighting", &history, 5).await,
        expected_edit_request("change the lighting", &["u1", "u2", "g5", "g6", "g7"])
    );
}

#[tokio::test]
async fn edit_preserves_a_generated_image_when_user_anchor_fills_the_limit() {
    let history = vec![
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: ["a", "b", "c", "d", "e"]
                .into_iter()
                .map(|image| ContentItem::InputImage {
                    image_url: format!("data:image/png;base64,{image}"),
                    detail: None,
                })
                .collect(),
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        generated_item("generated"),
    ];

    assert_eq!(
        edit_request("edit the last generated image", &history, 5).await,
        expected_edit_request(
            "edit the last generated image",
            &["b", "c", "d", "e", "generated"]
        )
    );
}

#[tokio::test]
async fn edit_uses_latest_user_upload_before_a_text_only_follow_up() {
    let history = vec![
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputImage {
                image_url: "data:image/png;base64,user".to_string(),
                detail: None,
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![
                ContentItem::InputText {
                    text: "edit this image".to_string(),
                },
                ContentItem::EncryptedContent {
                    encrypted_content: "encrypted".to_string(),
                },
            ],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
    ];

    assert_eq!(
        edit_request("change the lighting", &history, 1).await,
        expected_edit_request("change the lighting", &["user"])
    );
}

#[tokio::test]
async fn edit_reuses_images_from_prior_standalone_imagegen_calls() {
    let history = vec![
        ResponseItem::FunctionCall {
            id: None,
            name: IMAGEGEN_TOOL_NAME.to_string(),
            namespace: Some(IMAGE_GEN_NAMESPACE.to_string()),
            arguments: "{}".to_string(),
            call_id: "imagegen-1".to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
        generated_function_output("imagegen-1", "standalone"),
    ];

    assert_eq!(
        edit_request("change the lighting", &history, 1).await,
        expected_edit_request("change the lighting", &["standalone"])
    );
}

#[tokio::test]
async fn edit_keeps_newest_standalone_generated_images_when_over_limit() {
    let history = (1..=6)
        .flat_map(|index| {
            let call_id = format!("imagegen-{index}");
            vec![
                ResponseItem::FunctionCall {
                    id: None,
                    name: IMAGEGEN_TOOL_NAME.to_string(),
                    namespace: Some(IMAGE_GEN_NAMESPACE.to_string()),
                    arguments: "{}".to_string(),
                    call_id: call_id.clone(),
                    internal_chat_message_metadata_passthrough: None,
                },
                generated_function_output(&call_id, &index.to_string()),
            ]
        })
        .collect::<Vec<_>>();

    assert_eq!(
        edit_request("change the lighting", &history, 5).await,
        expected_edit_request("change the lighting", &["2", "3", "4", "5", "6"])
    );
}

fn input_image(image: &str) -> ContentItem {
    ContentItem::InputImage {
        image_url: format!("data:image/png;base64,{image}"),
        detail: None,
    }
}

async fn edit_request(prompt: &str, history: &[ResponseItem], count: usize) -> ImageEditRequest {
    let ImageRequest::Edit(request) = request_for_call_args(
        &ImagegenArgs {
            prompt: prompt.to_string(),
            referenced_image_paths: None,
            num_last_images_to_include: Some(count),
        },
        history,
        &[],
    )
    .await
    .expect("edit request should build") else {
        panic!("expected edit request");
    };
    request
}

fn image_output(image: &str) -> FunctionCallOutputPayload {
    FunctionCallOutputPayload::from_content_items(vec![FunctionCallOutputContentItem::InputImage {
        image_url: format!("data:image/png;base64,{image}"),
        detail: None,
    }])
}

fn expected_edit_request(prompt: &str, images: &[&str]) -> ImageEditRequest {
    ImageEditRequest {
        images: images
            .iter()
            .map(|image| ImageUrl {
                image_url: format!("data:image/png;base64,{image}"),
            })
            .collect(),
        prompt: prompt.to_string(),
        background: Some(ImageBackground::Auto),
        model: "gpt-image-2".to_string(),
        n: None,
        quality: Some(ImageQuality::Auto),
        size: Some("auto".to_string()),
    }
}

fn function_payload() -> ToolPayload {
    ToolPayload::Function {
        arguments: "{}".to_string(),
    }
}

fn generated_item(result: &str) -> ResponseItem {
    ResponseItem::ImageGenerationCall {
        id: Some(format!("id-{result}")),
        status: "completed".to_string(),
        revised_prompt: None,
        result: result.to_string(),
        internal_chat_message_metadata_passthrough: None,
    }
}

fn generated_function_output(call_id: &str, result: &str) -> ResponseItem {
    ResponseItem::FunctionCallOutput {
        call_id: call_id.to_string(),
        output: FunctionCallOutputPayload {
            body: FunctionCallOutputBody::ContentItems(vec![
                FunctionCallOutputContentItem::InputImage {
                    image_url: format!("data:image/png;base64,{result}"),
                    detail: Some(DEFAULT_IMAGE_DETAIL),
                },
                FunctionCallOutputContentItem::InputText {
                    text: "generated image save hint".to_string(),
                },
            ]),
            success: Some(true),
        },
        id: None,
        internal_chat_message_metadata_passthrough: None,
    }
}
