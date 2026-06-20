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

// tracing のマクロをインポート
use tracing::{info, warn, error, instrument};

// 設定を保持する構造体
struct Config {
    coolify_url: String,
    api_token: String,
    project_uuid: String,
    environment_name: String,
    server_uuid: String,
}

// 起動時に環境変数を一括チェックする関数
fn load_config() -> Config {
    Config {
        coolify_url: env::var("COOLIFY_URL").expect("環境変数 'COOLIFY_URL' が設定されていません"),
        api_token: env::var("COOLIFY_API_TOKEN").expect("環境変数 'COOLIFY_API_TOKEN' が設定されていません"),
        project_uuid: env::var("COOLIFY_PROJECT_UUID").expect("環境変数 'COOLIFY_PROJECT_UUID' が設定されていません"),
        environment_name: env::var("COOLIFY_ENVIRONMENT_NAME").unwrap_or_else(|_| "production".to_string()),
        server_uuid: env::var("COOLIFY_SERVER_UUID").expect("環境変数 'COOLIFY_SERVER_UUID' が設定されていません"),
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
    info!(target: "coolify_api", "Coolifyからアプリケーション一覧を取得中... URL: {}", url);
    let res = client.get(url).header("Authorization", auth_header).send().await;
    match res {
        Ok(response) => {
            if let Ok(apps) = response.json::<serde_json::Value>().await {
                if let Some(apps_array) = apps.as_array() {
                    let count = apps_array.len() as i32;
                    info!(target: "coolify_api", "アプリケーション数を取得成功: {}件", count);
                    return count;
                }
            }
            warn!(target: "coolify_api", "JSONのパース、または配列への変換に失敗しました。");
        }
        Err(e) => {
            error!(target: "coolify_api", "Coolify APIリクエストエラー: {:?}", e);
        }
    }
    0
}

#[async_trait]
impl EventHandler for Handler {
    // #[instrument] をつけることで、この関数内のログに自動的にインタラクションIDなどが付与されます
    #[instrument(skip(self, ctx, interaction))]
    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        let cfg = load_config();
        let client = reqwest::Client::new();
        let auth_header = format!("Bearer {}", cfg.api_token);

        match interaction {
            // ==================== 1. スラッシュコマンド ====================
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

                    let url = format!("{}/api/v1/applications", cfg.coolify_url);
                    let res = client.get(&url).header("Authorization", &auth_header).send().await;

                    if let Ok(response) = res {
                        if let Ok(apps) = response.json::<serde_json::Value>().await {
                            if let Some(apps_array) = apps.as_array() {
                                if apps_array.is_empty() {
                                    warn!("起動可能なアプリケーションがCoolify側に0件です。");
                                    let _ = command.edit_response(&ctx.http, EditInteractionResponse::new().content("❌ 起動できるアプリケーションが見つかりません。")).await;
                                    return;
                                }

                                let mut select_options = Vec::new();
                                for app in apps_array.iter().take(25) {
                                    let name = app["name"].as_str().unwrap_or("Unknown App").to_string();
                                    let uuid = app["uuid"].as_str().unwrap_or("").to_string();
                                    select_options.push(
                                        CreateSelectMenuOption::new(name, uuid.clone())
                                            .description(format!("UUID: {uuid}"))
                                    );
                                }

                                let menu = CreateSelectMenu::new("start_select", serenity::builder::CreateSelectMenuKind::String { options: select_options })
                                    .placeholder("起動するサーバーを選択してください");

                                let row = CreateActionRow::SelectMenu(menu);

                                if let Err(e) = command.edit_response(&ctx.http, EditInteractionResponse::new()
                                    .content("✨ 起動したいBasis Serverを選択してください：")
                                    .components(vec![row])
                                ).await {
                                    error!("セレクトメニュー送信エラー: {:?}", e);
                                }
                                return;
                            }
                        }
                    }
                    error!("Coolifyからのアプリケーション一覧取得、またはパースに失敗しました。");
                    let _ = command.edit_response(&ctx.http, EditInteractionResponse::new().content("❌ アプリケーション一覧の取得に失敗しました。")).await;
                }
            }

            // ==================== 2. モーダル送信時の処理 ====================
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

                    let list_url = format!("{}/api/v1/applications", cfg.coolify_url);
                    let offset = get_port_offset(&client, &list_url, &auth_header).await;
                    let current_set_port = BASE_SET_PORT + offset;
                    let current_health_port = BASE_HEALTH_PORT + offset;
                    let current_prom_port = BASE_PROM_PORT + offset;
                    let current_dashboard_port = BASE_DASHBOARD_PORT + offset;

                    info!("新規割り当てポート計算完了 -> SET: {}, HEALTH: {}, DASHBOARD: {}", current_set_port, current_health_port, current_dashboard_port);

                    let final_compose = DOCKER_COMPOSE_TEMPLATE
                        .replace("${SET_PORT}", &current_set_port.to_string())
                        .replace("${HEALTH_PORT}", &current_health_port.to_string())
                        .replace("${PROM_PORT}", &current_prom_port.to_string())
                        .replace("${DASHBOARD_PORT}", &current_dashboard_port.to_string());

                    let app_name = format!("basis-server-{}", current_set_port);

                    info!("Coolifyへアプリケーション登録リクエスト送信開始。名前: {}", app_name);
                    let create_url = format!("{}/api/v1/applications/docker-compose", cfg.coolify_url);
                    let create_res = client.post(&create_url)
                        .header("Authorization", &auth_header)
                        .json(&json!({
                            "project_uuid": cfg.project_uuid,
                            "environment_name": cfg.environment_name,
                            "server_uuid": cfg.server_uuid,
                            "name": app_name,
                            "docker_compose": final_compose.trim()
                        }))
                        .send()
                        .await;

                    match create_res {
                        Ok(res) if res.status().is_success() => {
                            let app_data: serde_json::Value = res.json().await.unwrap_or(json!({}));
                            let app_uuid = app_data["uuid"].as_str().unwrap_or_default();
                            info!("Coolifyへのアプリケーション登録成功。生成されたUUID: {}", app_uuid);

                            // 環境変数の設定
                            info!("環境変数 'Password' を登録中...");
                            let env_url = format!("{}/api/v1/applications/{}/envs", cfg.coolify_url, app_uuid);
                            let _ = client.post(&env_url)
                                .header("Authorization", &auth_header)
                                .json(&json!({
                                    "key": "Password",
                                    "value": admin_password,
                                    "is_build_time": false,
                                    "is_literal": true
                                }))
                                .send()
                                .await;

                            // デプロイのキック
                            info!("デプロイメントをキックします。UUID: {}", app_uuid);
                            let deploy_url = format!("{}/api/v1/applications/{}/deploy", cfg.coolify_url, app_uuid);
                            let _ = client.post(&deploy_url).header("Authorization", &auth_header).send().await;

                            let msg = format!(
                                "🚀 **Basis VR Server のデプロイを開始しました！**\n\
                                 🔹 **設定名称:** `{}`\n\
                                 🔹 **SetPort:** `{}` (UDP)\n\
                                 🔹 **HealthCheckPort:** `{}` (TCP)\n\
                                 🔹 **PromethusPort:** `{}` (TCP)\n\
                                 🔹 **DashboardPort:** `{}` (TCP)",
                                app_name, current_set_port, current_health_port, current_prom_port, current_dashboard_port
                            );
                            let _ = modal.edit_response(&ctx.http, EditInteractionResponse::new().content(msg)).await;
                        }
                        other => {
                            error!("Coolifyへのアプリケーション登録に失敗しました。レスポンス: {:?}", other);
                            let _ = modal.edit_response(&ctx.http, EditInteractionResponse::new().content("❌ Coolifyへのリソース登録に失敗しました。")).await;
                        }
                    }
                }
            }

            // ==================== 3. ドロップダウン選択時 ====================
            Interaction::Component(component) => {
                if component.data.custom_id == "start_select" {
                    if let serenity::model::application::ComponentInteractionDataKind::StringSelect { values } = &component.data.kind {
                        component.defer_ephemeral(&ctx.http).await.unwrap();

                        if let Some(selected_uuid) = values.first() {
                            info!("セレクトメニューよりサーバー起動リクエストを受信。対象UUID: {}", selected_uuid);
                            let deploy_url = format!("{}/api/v1/applications/{}/deploy", cfg.coolify_url, selected_uuid);
                            let deploy_res = client.post(&deploy_url).header("Authorization", &auth_header).send().await;

                            match deploy_res {
                                Ok(res) if res.status().is_success() => {
                                    info!("UUID: {} の再デプロイコマンド送信に成功しました。", selected_uuid);
                                    let _ = component.edit_response(&ctx.http, EditInteractionResponse::new()
                                        .content(format!("▶️ **アプリケーション (UUID: `{}`) の起動コマンドを送信しました！**", selected_uuid))
                                        .components(vec![]) 
                                    ).await;
                                }
                                other => {
                                    error!("UUID: {} の起動に失敗しました。原因: {:?}", selected_uuid, other);
                                    let _ = component.edit_response(&ctx.http, EditInteractionResponse::new().content("❌ アプリケーションの起動に失敗しました。")).await;
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
        let start_cmd = CreateCommand::new("start").description("既存のBasis Serverを選択して起動（再デプロイ）します");

        info!("グローバルスラッシュコマンドを登録中...");
        match Command::set_global_commands(&ctx.http, vec![deploy_cmd, start_cmd]).await {
            Ok(_) => info!("グローバルスラッシュコマンドの登録に成功しました。"),
            Err(e) => error!("グローバルスラッシュコマンドの登録に失敗しました: {:?}", e),
        }
    }
}

#[tokio::main]
async fn main() {
    // 💡 ログ出力の初期化 (環境変数 RUST_LOG が未設定ならデフォルトで info レベルを出力)
    if env::var("RUST_LOG").is_err() {
        env::set_var("RUST_LOG", "info");
    }
    tracing_subscriber::fmt::init();

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
