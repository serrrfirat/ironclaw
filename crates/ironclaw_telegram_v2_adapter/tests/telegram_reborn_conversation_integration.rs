//! Telegram v2 -> Reborn conversation integration harness.
//!
//! These tests use the Telegram tracer-bullet fixtures as realistic protocol
//! input for the durable `ironclaw_conversations` services. The Telegram crate
//! still stays payload-only in production; the workflow bridge below is a test
//! harness that mirrors the host seam.

use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use http::HeaderMap;
use ironclaw_conversations as conv;
use ironclaw_conversations::ConversationBindingService;
use ironclaw_host_api::{AgentId, ProjectId, TenantId, UserId};
use ironclaw_product_adapters as product;
use ironclaw_telegram_v2_adapter::{
    GroupTriggerPolicy, TelegramV2Adapter, TelegramV2AdapterConfig,
};
use ironclaw_turns::{
    CancelRunRequest, CancelRunResponse, GetRunStateRequest, ResumeTurnRequest, ResumeTurnResponse,
    RunProfileId, RunProfileVersion, SubmitTurnRequest, SubmitTurnResponse, TurnCoordinator,
    TurnError, TurnRunId, TurnRunState, TurnStatus,
};
use ironclaw_wasm_product_adapters::{
    NativeProductAdapterRunner, SharedSecretHeaderAuth, WebhookProcessOutcome, runner::WebhookAuth,
};

const FIXTURE_PATH: &str = "tests/fixtures";
const TELEGRAM_INSTALLATION_SUBJECT: &str = "telegram_install_alpha";
const TELEGRAM_WEBHOOK_SECRET: &str = "topsecret";

#[tokio::test]
async fn telegram_duplicate_update_replays_from_durable_conversation_state_after_restart() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("telegram-conversations.db");
    let coordinator = Arc::new(RecordingTurnCoordinator::default());

    let first_response = {
        let services = open_services(&db_path).await;
        services
            .pair_external_actor(
                tenant(),
                telegram_adapter_kind(),
                telegram_installation(),
                telegram_actor("777"),
                user("alice"),
            )
            .await
            .expect("pair actor");
        let workflow = Arc::new(DurableTelegramWorkflow::new(
            services.clone(),
            coordinator.clone(),
        ));
        let runner = build_runner(workflow.clone());

        let first = runner
            .process_webhook(
                &webhook_headers(Some(TELEGRAM_WEBHOOK_SECRET)),
                &fixture("private_chat_message.json"),
            )
            .await
            .expect("first delivery accepted");
        let WebhookProcessOutcome::Acknowledged { ack: first_ack } = first else {
            panic!("expected acknowledged first delivery");
        };
        assert!(matches!(
            first_ack,
            product::ProductInboundAck::Accepted { .. }
        ));
        assert_eq!(coordinator.submissions().len(), 1);

        let first_response = workflow.only_response();
        let target = services
            .validate_reply_target(conv::ValidateReplyTargetRequest {
                tenant_id: tenant(),
                actor_user_id: user("alice"),
                adapter_kind: telegram_adapter_kind(),
                adapter_installation_id: telegram_installation(),
                external_actor_ref: telegram_actor("777"),
                current_thread_id: first_response.resolution.turn_scope.thread_id.clone(),
                reply_target_binding_ref: first_response
                    .accepted_message
                    .reply_target_binding_ref
                    .clone(),
            })
            .await
            .expect("reply target validates");
        assert_eq!(target.external_conversation_ref.conversation_id(), "777");
        assert_eq!(target.external_conversation_ref.message_id(), Some("11"));
        first_response
    };

    let services = open_services(&db_path).await;
    services
        .unpair_external_actor(
            &tenant(),
            &telegram_adapter_kind(),
            &telegram_installation(),
            &telegram_actor("777"),
        )
        .await
        .expect("remove live pairing after restart");
    let workflow = Arc::new(DurableTelegramWorkflow::new(
        services.clone(),
        coordinator.clone(),
    ));
    let runner = build_runner(workflow.clone());

    let second = runner
        .process_webhook(
            &webhook_headers(Some(TELEGRAM_WEBHOOK_SECRET)),
            &fixture("duplicate_update.json"),
        )
        .await
        .expect("duplicate delivery replays after restart");
    let WebhookProcessOutcome::Acknowledged { ack: second_ack } = second else {
        panic!("expected acknowledged duplicate delivery");
    };
    let product::ProductInboundAck::Duplicate { prior } = second_ack else {
        panic!("expected duplicate ack, got {second_ack:?}");
    };
    assert!(matches!(
        *prior,
        product::ProductInboundAck::Accepted { .. }
    ));
    assert_eq!(
        coordinator.submissions().len(),
        1,
        "durable duplicate replay must not submit a second turn"
    );

    let replayed_response = workflow.only_response();
    assert_eq!(
        replayed_response.accepted_message.idempotency,
        conv::MessageIdempotencyStatus::Duplicate
    );
    assert_eq!(
        replayed_response.accepted_message.message_ref,
        first_response.accepted_message.message_ref
    );
    assert_eq!(
        replayed_response.accepted_message.received_at, first_response.accepted_message.received_at,
        "duplicate replay must preserve the originally accepted Telegram timestamp"
    );
}

fn fixture(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(FIXTURE_PATH)
        .join(name);
    std::fs::read(&path).unwrap_or_else(|err| panic!("read fixture {name}: {err}"))
}

async fn open_services(path: &Path) -> conv::RebornLibSqlConversationServices {
    let db = Arc::new(
        libsql::Builder::new_local(path.display().to_string())
            .build()
            .await
            .expect("build libsql database"),
    );
    conv::RebornLibSqlConversationServices::new(db)
        .await
        .expect("open conversation services")
}

fn build_runner(workflow: Arc<dyn product::ProductWorkflow>) -> NativeProductAdapterRunner {
    let adapter: Arc<dyn product::ProductAdapter> = Arc::new(TelegramV2Adapter::new(config()));
    NativeProductAdapterRunner::new(
        adapter,
        workflow,
        WebhookAuth::SharedSecretHeader(SharedSecretHeaderAuth {
            header_name: "X-Telegram-Bot-Api-Secret-Token".into(),
            expected_secret: TELEGRAM_WEBHOOK_SECRET.into(),
            subject: TELEGRAM_INSTALLATION_SUBJECT.into(),
        }),
    )
}

fn config() -> TelegramV2AdapterConfig {
    TelegramV2AdapterConfig {
        adapter_id: product::ProductAdapterId::new("telegram_v2").expect("valid"),
        installation_id: product::AdapterInstallationId::new(TELEGRAM_INSTALLATION_SUBJECT)
            .expect("valid"),
        group_trigger_policy: GroupTriggerPolicy {
            bot_username: "ironclaw_bot".into(),
            bot_user_id: 9000,
            recognized_commands: vec!["help".into(), "start".into()],
        },
        egress_credential_handle: product::EgressCredentialHandle::new("telegram_bot_token")
            .expect("valid"),
        progress_push_enabled: false,
    }
}

fn webhook_headers(secret: Option<&str>) -> HeaderMap {
    let mut map = HeaderMap::new();
    if let Some(secret) = secret {
        map.insert(
            http::header::HeaderName::from_static("x-telegram-bot-api-secret-token"),
            http::header::HeaderValue::from_str(secret).expect("header value"),
        );
    }
    map
}

struct DurableTelegramWorkflow<C> {
    inbound: conv::InboundTurnService<
        conv::RebornLibSqlConversationServices,
        conv::RebornLibSqlConversationServices,
        C,
    >,
    responses: Mutex<Vec<conv::InboundTurnResponse>>,
}

impl<C> DurableTelegramWorkflow<C>
where
    C: TurnCoordinator,
{
    fn new(services: conv::RebornLibSqlConversationServices, coordinator: Arc<C>) -> Self {
        Self {
            inbound: conv::InboundTurnService::new(services.clone(), services, coordinator),
            responses: Mutex::new(Vec::new()),
        }
    }

    fn only_response(&self) -> conv::InboundTurnResponse {
        let responses = self.responses.lock().expect("responses lock");
        assert_eq!(responses.len(), 1, "expected exactly one workflow response");
        responses[0].clone()
    }
}

#[async_trait]
impl<C> product::ProductWorkflow for DurableTelegramWorkflow<C>
where
    C: TurnCoordinator + 'static,
{
    async fn accept_inbound(
        &self,
        envelope: product::ProductInboundEnvelope,
    ) -> Result<product::ProductInboundAck, product::ProductAdapterError> {
        let response = self
            .inbound
            .handle_inbound_turn(inbound_request_from_envelope(envelope)?)
            .await
            .map_err(map_inbound_error)?;
        let ack = product_ack_from_response(&response);
        self.responses
            .lock()
            .expect("responses lock")
            .push(response);
        Ok(ack)
    }
}

fn inbound_request_from_envelope(
    envelope: product::ProductInboundEnvelope,
) -> Result<conv::InboundTurnRequest, product::ProductAdapterError> {
    let external_event_id = envelope.external_event_id.as_str().to_string();
    Ok(conv::InboundTurnRequest {
        tenant_id: tenant(),
        adapter_kind: conv::AdapterKind::new(envelope.adapter_id.as_str().to_string())
            .map_err(map_identifier_error)?,
        adapter_installation_id: conv::AdapterInstallationId::new(
            envelope.installation_id.as_str().to_string(),
        )
        .map_err(map_identifier_error)?,
        external_actor_ref: conv::ExternalActorRef::new(
            envelope.external_actor_ref.kind().to_string(),
            envelope.external_actor_ref.id().to_string(),
        )
        .map_err(map_identifier_error)?,
        external_conversation_ref: conv::ExternalConversationRef::new(
            envelope.external_conversation_ref.space_id(),
            envelope
                .external_conversation_ref
                .conversation_id()
                .to_string(),
            envelope.external_conversation_ref.topic_id(),
            envelope.external_conversation_ref.reply_target_message_id(),
        )
        .map_err(map_identifier_error)?,
        external_event_id: conv::ExternalEventId::new(external_event_id.clone())
            .map_err(map_identifier_error)?,
        route_kind: route_kind_for_payload(&envelope.payload),
        content_ref: conv::InboundMessageContentRef::new(format!(
            "telegram-fixture-content:{external_event_id}"
        ))
        .map_err(map_identifier_error)?,
        requested_agent_id: Some(agent()),
        requested_project_id: Some(project()),
        received_at: envelope.received_at,
        requested_run_profile: None,
    })
}

fn route_kind_for_payload(payload: &product::ProductInboundPayload) -> conv::ConversationRouteKind {
    let trigger = match payload {
        product::ProductInboundPayload::UserMessage(message) => Some(message.trigger),
        product::ProductInboundPayload::Command(command) => Some(command.trigger),
        _ => None,
    };
    match trigger {
        Some(product::ProductTriggerReason::DirectChat) => conv::ConversationRouteKind::Direct,
        Some(_) => conv::ConversationRouteKind::Shared,
        None => conv::ConversationRouteKind::Direct,
    }
}

fn product_ack_from_response(response: &conv::InboundTurnResponse) -> product::ProductInboundAck {
    let accepted_message_ref = response.accepted_message.message_ref.as_str().to_string();
    let submitted_run_id = response
        .turn_submission
        .as_ref()
        .map(|submission| match submission {
            SubmitTurnResponse::Accepted { run_id, .. } => *run_id,
        });
    let accepted = product::ProductInboundAck::Accepted {
        accepted_message_ref,
        submitted_run_id,
    };
    if response.accepted_message.idempotency == conv::MessageIdempotencyStatus::Duplicate {
        product::ProductInboundAck::Duplicate {
            prior: Box::new(accepted),
        }
    } else {
        accepted
    }
}

fn map_identifier_error(error: conv::InboundTurnError) -> product::ProductAdapterError {
    product::ProductAdapterError::WorkflowRejected {
        reason: error.to_string(),
    }
}

fn map_inbound_error(error: conv::InboundTurnError) -> product::ProductAdapterError {
    match error {
        conv::InboundTurnError::BindingRequired { .. } => {
            product::ProductAdapterError::WorkflowRejected {
                reason: "binding required".into(),
            }
        }
        conv::InboundTurnError::TurnSubmissionFailed { error } => {
            product::ProductAdapterError::WorkflowTransient {
                reason: error.to_string(),
            }
        }
        other => product::ProductAdapterError::WorkflowRejected {
            reason: other.to_string(),
        },
    }
}

fn tenant() -> TenantId {
    TenantId::new("tenant-a").expect("valid tenant")
}

fn user(id: &str) -> UserId {
    UserId::new(id).expect("valid user")
}

fn agent() -> AgentId {
    AgentId::new("agent-a").expect("valid agent")
}

fn project() -> ProjectId {
    ProjectId::new("project-a").expect("valid project")
}

fn telegram_adapter_kind() -> conv::AdapterKind {
    conv::AdapterKind::new("telegram_v2").expect("valid adapter kind")
}

fn telegram_installation() -> conv::AdapterInstallationId {
    conv::AdapterInstallationId::new(TELEGRAM_INSTALLATION_SUBJECT).expect("valid installation")
}

fn telegram_actor(id: &str) -> conv::ExternalActorRef {
    conv::ExternalActorRef::new("telegram_user", id).expect("valid actor")
}

#[derive(Default)]
struct RecordingTurnCoordinator {
    submissions: Mutex<Vec<SubmitTurnRequest>>,
}

impl RecordingTurnCoordinator {
    fn submissions(&self) -> Vec<SubmitTurnRequest> {
        self.submissions.lock().expect("submissions lock").clone()
    }
}

#[async_trait]
impl TurnCoordinator for RecordingTurnCoordinator {
    async fn submit_turn(
        &self,
        request: SubmitTurnRequest,
    ) -> Result<SubmitTurnResponse, TurnError> {
        self.submissions
            .lock()
            .expect("submissions lock")
            .push(request.clone());
        Ok(SubmitTurnResponse::Accepted {
            turn_id: ironclaw_turns::TurnId::new(),
            run_id: TurnRunId::new(),
            status: TurnStatus::Queued,
            resolved_run_profile_id: RunProfileId::default_profile(),
            resolved_run_profile_version: RunProfileVersion::new(1),
            event_cursor: ironclaw_turns::events::EventCursor(1),
            accepted_message_ref: request.accepted_message_ref,
            reply_target_binding_ref: request.reply_target_binding_ref,
        })
    }

    async fn resume_turn(
        &self,
        _request: ResumeTurnRequest,
    ) -> Result<ResumeTurnResponse, TurnError> {
        unimplemented!("not used by Telegram conversation harness")
    }

    async fn cancel_run(&self, _request: CancelRunRequest) -> Result<CancelRunResponse, TurnError> {
        unimplemented!("not used by Telegram conversation harness")
    }

    async fn get_run_state(&self, _request: GetRunStateRequest) -> Result<TurnRunState, TurnError> {
        unimplemented!("not used by Telegram conversation harness")
    }
}
