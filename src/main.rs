use serde_json::json;
use serenity::async_trait;
use serenity::builder::{
    CreateActionRow, CreateCommand, CreateInteractionResponse, CreateInputText, CreateModal,
    CreateSelectMenu, CreateSelectMenuOption, EditInteractionResponse,
};
use serenity::model::application::{ActionRowComponent, Command, Interaction, InputTextStyle};
use serenity::model::gateway::Ready;
use serenity::prelude::*;
use std::env;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use tracing::{info, warn, error, instrument};

struct Config {
    coolify_url: String,
    api_token: String,
    project_uuid: String,
    environment_name: String,
    server_uuid: String,
    destination_uuid: String,
}

fn load_config() -> Config {
    Config {
        coolify_url: env::var("COOLIFY_URL").expect("環境変数 'COOLIFY_URL' が設定されていません"),
        api_token: env::var("COOLIFY_API_TOKEN").expect("環境変数 'COOLIFY_API_TOKEN' が設定されていません"),
        project_uuid: env::var("COOLIFY_PROJECT_UUID").expect("環境変数 'COOLIFY_PROJECT_UUID' が設定されていません"),
        environment_name: env::var("COOLIFY_ENVIRONMENT_NAME").unwrap_or_else(|_| "production".to_string()),
        server_uuid: env::var("COOLIFY_SERVER_UUID").expect("環境変数 'COOLIFY_SERVER_UUID' が設定されていません"),
        destination_uuid: env::var("COOLIFY_DESTINATION_UUID").expect("環境変数 'COOLIFY_DESTINATION_UUID' が設定されていません"),
    }
}

const BASE_SET_PORT: i32 = 4296;
const BASE_HEALTH_PORT: i32 = 10666;
const BASE_PROM_PORT: i32 = 1234;
const BASE_DASHBOARD_PORT: i32 = 4000;

const DOCKER_COMPOSE_TEMPLATE: &str = r#"
version: '3.8'
services:
  basis-server:
    image: 'ghcr.io/basisvr/basis-server:nightly'
    container_name: basis-server-${SET_PORT}
    init: true
    restart: unless-stopped
    environment:
      SetPort: ${SET_PORT}
      HealthCheckPort: ${HEALTH_PORT}
      PromethusPort: ${PROM_PORT}
      PeerLimit: 1024
      EnableStatistics: true
      EnableConsole: false
    ports:
      - '${SET_PORT}:${SET_PORT}/udp'
      - '${HEALTH_PORT}:${HEALTH_PORT}/tcp'
      - '${PROM_PORT}:${PROM_PORT}/tcp'
      - '${DASHBOARD_PORT}:${DASHBOARD_PORT}/tcp'
    volumes:
      - './initialresources:/app/initialresources:ro'
      - './config:/app/config'
      - './logs:/app/logs'
    healthcheck:
      test: ["CMD", "curl", "-f", "http://localhost:${HEALTH_PORT}/health"]
      interval: 30s
      timeout: 10s
      retries: 3
      start_period: 2s
  marusansi-basis-dashboard:
    image: 'nodokaha/basis-dashboard:latest'
    container_name: dashboard-${SET_PORT}
    restart: unless-stopped
    network_mode: 'service:basis-server'
    environment:
      PORT: ${DASHBOARD_PORT}
    depends_on:
      basis-server:
        condition: service_healthy
"#;

struct Handler;

async fn get_port_offset(client: &reqwest::Client, url: &str, auth_header: &str) -> i32 {
    info!(target: "coolify_api", "Coolifyから既存サービス一覧を取得中... URL: {}", url);
    let res = client.get(url).header("Authorization", auth_header).send().await;
    match res {
        Ok(response) => {
            let status = response.status();
            let body_text = response.text().await.unwrap_or_else(|_| "ボディの読み込みに失敗".to_string());
            
            if status.is_success() {
                if let Ok(services) = serde_json::from_str::<serde_json::Value>(&body_text) {
                    if let Some(services_array) = services.as_array() {
                        let count = services_array.len() as i32;
                        info!(target: "coolify_api", "サービス数を取得成功: {}件", count);
                        return count;
                    }
                }
                warn!(target: "coolify_api", "サービス一覧のJSONパースに失敗しました。生データ: {}", body_text);
            } else {
                error!(target: "coolify_api", "サービス一覧取得失敗。ステータス: {}, レスポンス: {}", status, body_text);
            }
        }
        Err(e) => {
            error!(target: "coolify_api", "Coolify APIリクエストエラー(通信失敗): {:?}", e);
        }
    }
    0
}

#[async_trait]
impl EventHandler for Handler {
    #[instrument(skip(self, ctx, interaction))]
    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        let cfg = load_config();
        let client = reqwest::Client::new();
        let auth_header = format!("Bearer {}", cfg.api_token);

        match interaction {
            Interaction::Command(command) => {
                let user_name = &command.user.name;
                let command_name = &command.data.name;
                info!("コマンド実行: /{} (実行者: {})", command_name, user_name);

                if command_name == "deploy" {
                    let password_input = CreateInputText::new(
                        InputTextStyle::Short,
                        "サーバーのパスワード (ADMIN_PASSWORD)",
                        "admin_password",
                    )
                    .placeholder("パスワードを入力してください")
                    .required(true);

                    let modal = CreateModal::new("deploy_modal", "Basis Server 自動デプロイ設定")
                        .components(vec![CreateActionRow::InputText(password_input)]);

                    let response = CreateInteractionResponse::Modal(modal);
                    if let Err(e) = command.create_response(&ctx.http, response).await {
                        error!("モーダル送信エラー: {:?}", e);
                    }
                }
                
                else if command_name == "start" {
                    if let Err(e) = command.defer_ephemeral(&ctx.http).await {
                        error!("defer_ephemeral エラー: {:?}", e);
                        return;
                    }

                    let url = format!("{}/api/v1/services", cfg.coolify_url);
                    let res = client.get(&url).header("Authorization", &auth_header).send().await;

                    match res {
                        Ok(response) => {
                            let status = response.status();
                            let body_text = response.text().await.unwrap_or_else(|_| "ボディの読み込みに失敗".to_string());

                            if status.is_success() {
                                if let Ok(services) = serde_json::from_str::<serde_json::Value>(&body_text) {
                                    if let Some(services_array) = services.as_array() {
                                        if services_array.is_empty() {
                                            warn!("起動可能なサービスがCoolify側に0件です。");
                                            let _ = command.edit_response(&ctx.http, EditInteractionResponse::new().content("❌ 起動できるサービスが見つかりません。")).await;
                                            return;
                                        }

                                        let mut select_options = Vec::new();
                                        for service in services_array.iter().take(25) {
                                            let name = service["name"].as_str().unwrap_or("Unknown Service").to_string();
                                            let uuid = service["uuid"].as_str().unwrap_or("").to_string();
                                            select_options.push(
                                                CreateSelectMenuOption::new(name, uuid.clone())
                                                    .description(format!("UUID: {uuid}"))
                                            );
                                        }

                                        let menu = CreateSelectMenu::new("start_select", serenity::builder::CreateSelectMenuKind::String { options: select_options })
                                            .placeholder("起動するサービスを選択してください");

                                        let row = CreateActionRow::SelectMenu(menu);

                                        let _ = command.edit_response(&ctx.http, EditInteractionResponse::new()
                                            .content("✨ 起動したいBasis Server（サービス）を選択してください：")
                                            .components(vec![row])
                                        ).await;
                                        return;
                                    }
                                }
                                error!("サービス一覧のJSONパースに失敗。生データ: {}", body_text);
                            } else {
                                error!("Coolifyからのサービス一覧取得に失敗。ステータス: {}, レスポンス: {}", status, body_text);
                            }
                        }
                        Err(e) => error!("Coolifyとの通信に失敗しました: {:?}", e),
                    }
                    let _ = command.edit_response(&ctx.http, EditInteractionResponse::new().content("❌ サービス一覧の取得に失敗しました。")).await;
                }
            }

            Interaction::Modal(modal) => {
                let user_name = &modal.user.name;
                info!("モーダル送信を受信: custom_id={} (送信者: {})", modal.data.custom_id, user_name);

                if modal.data.custom_id == "deploy_modal" {
                    modal.defer_ephemeral(&ctx.http).await.unwrap();

                    let mut admin_password = String::new();
                    for row in &modal.data.components {
                        if let ActionRowComponent::InputText(input) = &row.components[0] {
                            if input.custom_id == "admin_password" {
                                admin_password = input.value.clone().unwrap_or_default();
                            }
                        }
                    }

                    let list_url = format!("{}/api/v1/services", cfg.coolify_url);
                    let offset = get_port_offset(&client, &list_url, &auth_header).await;
                    let current_set_port = BASE_SET_PORT + offset;
                    let current_health_port = BASE_HEALTH_PORT + offset;
                    let current_prom_port = BASE_PROM_PORT + offset;
                    let current_dashboard_port = BASE_DASHBOARD_PORT + offset;

                    info!("新規ポート計算結果 -> SET: {}, HEALTH: {}", current_set_port, current_health_port);

                    let final_compose = DOCKER_COMPOSE_TEMPLATE
                        .replace("${SET_PORT}", &current_set_port.to_string())
                        .replace("${HEALTH_PORT}", &current_health_port.to_string())
                        .replace("${PROM_PORT}", &current_prom_port.to_string())
                        .replace("${DASHBOARD_PORT}", &current_dashboard_port.to_string());

                    let base64_compose = STANDARD.encode(final_compose.trim());
                    let app_name = format!("basis-server-{}", current_set_port);
                    let create_url = format!("{}/api/v1/services", cfg.coolify_url);
                    info!("Coolifyへサービス作成リクエスト送信(Base64化)。URL: {}", create_url);

                    let create_res = client.post(&create_url)
                        .header("Authorization", &auth_header)
                        .json(&json!({
                            "name": app_name,
                            "project_uuid": cfg.project_uuid,
                            "server_uuid": cfg.server_uuid,
                            "destination_uuid": cfg.destination_uuid,
                            "environment_name": cfg.environment_name,
                            "docker_compose_raw": base64_compose 
                        }))
                        .send()
                        .await;

                    match create_res {
                        Ok(res) => {
                            let status = res.status();
                            let body_text = res.text().await.unwrap_or_else(|_| "ボディの読み込みに失敗".to_string());

                            if status.is_success() || status.as_u16() == 201 {
                                if let Ok(app_data) = serde_json::from_str::<serde_json::Value>(&body_text) {
                                    let service_uuid = app_data["uuid"].as_str().unwrap_or_default();
                                    info!("Coolifyへのサービス登録成功。生成UUID: {}", service_uuid);

                                    let env_url = format!("{}/api/v1/services/{}/envs", cfg.coolify_url, service_uuid);
                                    info!("環境変数 'Password' を登録中... URL: {}", env_url);
                                    
                                    let env_res = client.post(&env_url)
                                        .header("Authorization", &auth_header)
                                        .json(&json!({
                                            "key": "Password",
                                            "value": admin_password,
                                            "is_preview": false,
                                            "is_literal": true
                                        }))
                                        .send()
                                        .await;
                                    
                                    if let Ok(e_res) = env_res {
                                        let e_status = e_res.status();
                                        let e_body = e_res.text().await.unwrap_or_default();
                                        info!("環境変数登録ステータス: {}, レスポンス: {}", e_status, e_body);
                                    }

                                    info!("サービスのデプロイメントを開始します。UUID: {}", service_uuid);
                                    let deploy_url = format!("{}/api/v1/services/{}/deploy", cfg.coolify_url, service_uuid);
                                    let _ = client.post(&deploy_url).header("Authorization", &auth_header).send().await;

                                    let msg = format!(
                                        "🚀 **Basis VR Server のデプロイを開始しました！(Services仕様)**\n\
                                         🔹 **設定名称:** `{}`\n\
                                         🔹 **SetPort:** `{}` (UDP)\n\
                                         🔹 **HealthCheckPort:** `{}` (TCP)\n\
                                         🔹 **DashboardPort:** `{}` (TCP)",
                                        app_name, current_set_port, current_health_port, current_dashboard_port
                                    );
                                    let _ = modal.edit_response(&ctx.http, EditInteractionResponse::new().content(msg)).await;
                                    return;
                                }
                                error!("サービス作成レスポンスのパースに失敗。生データ: {}", body_text);
                            } else {
                                error!("Coolifyへのサービス登録に失敗。ステータス: {}, レスポンス: {}", status, body_text);
                            }
                        }
                        Err(e) => {
                            error!("Coolifyサービス登録通信時に致命的エラー: {:?}", e);
                        }
                    }
                    let _ = modal.edit_response(&ctx.http, EditInteractionResponse::new().content("❌ Coolifyへのサービス登録に失敗しました。詳細ログを確認してください。")).await;
                }
            }

            Interaction::Component(component) => {
                if component.data.custom_id == "start_select" {
                    if let serenity::model::application::ComponentInteractionDataKind::StringSelect { values } = &component.data.kind {
                        component.defer_ephemeral(&ctx.http).await.unwrap();

                        if let Some(selected_uuid) = values.first() {
                            info!("セレクトメニューよりサービス起動リクエストを受信。対象UUID: {}", selected_uuid);
                            
                            let start_url = format!("{}/api/v1/services/{}/start", cfg.coolify_url, selected_uuid);
                            info!("Coolifyへサービス起動リクエスト(GET)送信。URL: {}", start_url);
                            
                            let start_res = client.get(&start_url).header("Authorization", &auth_header).send().await;

                            match start_res {
                                Ok(res) => {
                                    let status = res.status();
                                    let body_text = res.text().await.unwrap_or_else(|_| "ボディの読み込みに失敗".to_string());

                                    if status.is_success() {
                                        info!("サービス UUID: {} の起動リクエスト送信成功。レスポンス: {}", selected_uuid, body_text);
                                        let _ = component.edit_response(&ctx.http, EditInteractionResponse::new()
                                            .content(format!("▶️ **サービス (UUID: `{}`) の起動リクエストを受理しました！ (タスクがキューに追加されました)**", selected_uuid))
                                            .components(vec![])
                                        ).await;
                                    } else {
                                        error!("サービス UUID: {} の起動に失敗。ステータス: {}, レスポンス: {}", selected_uuid, status, body_text);
                                        let _ = component.edit_response(&ctx.http, EditInteractionResponse::new().content("❌ サービスの起動に失敗しました。Coolify側のログを確認してください。")).await;
                                    }
                                }
                                Err(e) => {
                                    error!("サービス UUID: {} の起動通信エラー: {:?}", selected_uuid, e);
                                    let _ = component.edit_response(&ctx.http, EditInteractionResponse::new().content("❌ 通信エラーにより起動に失敗しました。")).await;
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    async fn ready(&self, ctx: Context, ready: Ready) {
        info!("Botが正常に起動しました！ログイン名: {}", ready.user.name);
        
        let deploy_cmd = CreateCommand::new("deploy").description("Basis Serverをポート自動スライドで新規デプロイします");
        let start_cmd = CreateCommand::new("start").description("既存のBasis Serverを選択して起動（スタート）します");

        info!("グローバルスラッシュコマンドを登録中...");
        match Command::set_global_commands(&ctx.http, vec![deploy_cmd, start_cmd]).await {
            Ok(_) => info!("グローバルスラッシュコマンドの登録に成功しました。"),
            Err(e) => error!("グローバルスラッシュコマンドの登録に失敗しました: {:?}", e),
        }
    }
}

#[tokio::main]
async fn main() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .init();

    info!("Botの初期化を開始します...");

    let token = env::var("DISCORD_BOT_TOKEN").expect("環境変数 'DISCORD_BOT_TOKEN' が設定されていません");
    let _ = load_config();

    let intents = GatewayIntents::empty();
    let mut client = Client::builder(&token, intents).event_handler(Handler).await.expect("Err");
    
    info!("Discordゲートウェイに接続しています...");
    if let Err(why) = client.start().await { 
        error!("Botの実行中にエラーが発生しました: {:?}", why); 
    }
}
