use serde_json::json;
use serenity::async_trait;
use serenity::model::application::{Command, Interaction, ActionRowComponent, ComponentType};
use serenity::model::gateway::Ready;
use serenity::prelude::*;
use std::env;

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
    let res = client.get(url).header("Authorization", auth_header).send().await;
    if let Ok(response) = res {
        if let Ok(apps) = response.json::<serde_json::Value>().await {
            if let Some(apps_array) = apps.as_array() {
                return apps_array.len() as i32;
            }
        }
    }
    0
}

#[async_trait]
impl EventHandler for Handler {
    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        // ハンドラが呼ばれるたびに環境変数をロード（最新の設定を反映可能に）
        let cfg = load_config();
        let client = reqwest::Client::new();
        let auth_header = format!("Bearer {}", cfg.api_token);

        match interaction {
            // ==================== 1. スラッシュコマンド ====================
            Interaction::Command(command) => {
                if command.data.name == "deploy" {
                    let response = json!({
                        "type": 9,
                        "data": {
                            "custom_id": "deploy_modal",
                            "title": "Basis Server 自動デプロイ設定",
                            "components": [{
                                "type": 1,
                                "components": [{
                                    "type": 4,
                                    "custom_id": "admin_password",
                                    "label": "サーバーのパスワード (ADMIN_PASSWORD)",
                                    "style": 1,
                                    "placeholder": "パスワードを入力してください",
                                    "required": true
                                }]
                            }]
                        }
                    });
                    let _ = command.create_response(&ctx.http, serde_json::from_value(response).unwrap()).await;
                }
                
                else if command.data.name == "start" {
                    command.defer_ephemeral(&ctx.http).await.unwrap();

                    let url = format!("{}/api/v1/applications", cfg.coolify_url);
                    let res = client.get(&url).header("Authorization", &auth_header).send().await;

                    if let Ok(response) = res {
                        if let Ok(apps) = response.json::<serde_json::Value>().await {
                            if let Some(apps_array) = apps.as_array() {
                                if apps_array.is_empty() {
                                    command.edit_response(&ctx.http, |m| m.content("❌ 起動できるアプリケーションが見つかりません。")).await.unwrap();
                                    return;
                                }

                                let mut options = Vec::new();
                                for app in apps_array.iter().take(25) {
                                    let name = app["name"].as_str().unwrap_or("Unknown App");
                                    let uuid = app["uuid"].as_str().unwrap_or("");
                                    options.push(json!({
                                        "label": name,
                                        "value": uuid,
                                        "description": format!("UUID: {uuid}")
                                    }));
                                }

                                let menu_component = json!({
                                    "type": 1,
                                    "components": [{
                                        "type": 3,
                                        "custom_id": "start_select",
                                        "options": options,
                                        "placeholder": "起動するサーバーを選択してください"
                                    }]
                                });

                                command.edit_response(&ctx.http, |m| {
                                    m.content("✨ 起動したいBasis Serverを選択してください：")
                                     .components(|c| {
                                         *c = serde_json::from_value(menu_component).unwrap();
                                         c
                                     })
                                }).await.unwrap();
                                return;
                            }
                        }
                    }
                    command.edit_response(&ctx.http, |m| m.content("❌ アプリケーション一覧の取得に失敗しました。")).await.unwrap();
                }
            }

            // ==================== 2. モーダル送信時の処理 ====================
            Interaction::ModalSubmit(modal) => {
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

                    let final_compose = DOCKER_COMPOSE_TEMPLATE
                        .replace("${SET_PORT}", &current_set_port.to_string())
                        .replace("${HEALTH_PORT}", &current_health_port.to_string())
                        .replace("${PROM_PORT}", &current_prom_port.to_string())
                        .replace("${DASHBOARD_PORT}", &current_dashboard_port.to_string());

                    let app_name = format!("basis-server-{}", current_set_port);

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

                            let deploy_url = format!("{}/api/v1/applications/{}/deploy", cfg.coolify_url, app_uuid);
                            let _ = client.post(&deploy_url).header("Authorization", &auth_header).send().await;

                            let msg = format!(
                                "🚀 **Basis VR Server のデプロイを開始しました！**\n\
                                 🔹 **設定名称:** `{}`\n\
                                 🔹 **SetPort:** `{}` (UDP)\n\
                                 🔹 **HealthCheckPort:** `{}` (TCP)\n\
                                 🔹 **PromethusPort:** `{}` (TCP)",
                                app_name, current_set_port, current_health_port, current_prom_port
                            );
                            modal.edit_response(&ctx.http, |m| m.content(msg)).await.unwrap();
                        }
                        _ => {
                            modal.edit_response(&ctx.http, |m| m.content("❌ Coolifyへのリソース登録に失敗しました。")).await.unwrap();
                        }
                    }
                }
            }

            // ==================== 3. ドロップダウン選択時 ====================
            Interaction::Component(component) => {
                if component.data.custom_id == "start_select" && component.data.component_type == ComponentType::StringSelect {
                    component.defer_ephemeral(&ctx.http).await.unwrap();

                    if let Some(selected_uuid) = component.data.values.first() {
                        let deploy_url = format!("{}/api/v1/applications/{}/deploy", cfg.coolify_url, selected_uuid);
                        let deploy_res = client.post(&deploy_url).header("Authorization", &auth_header).send().await;

                        match deploy_res {
                            Ok(res) if res.status().is_success() => {
                                component.edit_response(&ctx.http, |m| {
                                    m.content(format!("▶️ **アプリケーション (UUID: `{}`) の起動コマンドを送信しました！**", selected_uuid))
                                     .components(|c| c)
                                }).await.unwrap();
                            }
                            _ => {
                                component.edit_response(&ctx.http, |m| m.content("❌ アプリケーションの起動に失敗しました。")).await.unwrap();
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    async fn ready(&self, ctx: Context, ready: Ready) {
        println!("{} is ready.", ready.user.name);
        
        let _ = Command::create_global_command(&ctx.http, |command| {
            command.name("deploy").description("Basis Serverをポート自動スライドで新規デプロイします")
        }).await;

        let _ = Command::create_global_command(&ctx.http, |command| {
            command.name("start").description("既存のBasis Serverを選択して起動（再デプロイ）します")
        }).await;
    }
}

#[tokio::main]
async fn main() {
    // 起動時にDiscordトークンがなければ即エラー終了
    let token = env::var("DISCORD_BOT_TOKEN").expect("環境変数 'DISCORD_BOT_TOKEN' が設定されていません");
    
    // 他のCoolify用環境変数も起動時にチェックしておく
    let _ = load_config();

    let intents = GatewayIntents::empty();
    let mut client = Client::builder(&token, intents).event_handler(Handler).await.expect("Err");
    if let Err(why) = client.start().await { println!("Error: {:?}", why); }
}
