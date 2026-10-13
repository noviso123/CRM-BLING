use axum::{
    Form,
    extract::{Query, State},
    http::{HeaderMap, header},
    response::{IntoResponse, Redirect, Response},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{TimeZone, Utc};
use hmac::{Hmac, Mac};
use maud::{DOCTYPE, Markup, PreEscaped, html};
use serde::Deserialize;
use sha2::Sha256;
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::{AppState, Config, db};

type HmacSha256 = Hmac<Sha256>;
const SESSION_SECONDS: i64 = 8 * 60 * 60;
const CSS: &str = include_str!("../static/app.css");

#[derive(Default, Deserialize)]
pub struct HomeQuery {
    phone: Option<String>,
    q: Option<String>,
    sent: Option<String>,
    template_sent: Option<String>,
    contact_saved: Option<String>,
    error: Option<String>,
}

#[derive(Deserialize)]
pub struct LoginForm {
    password: String,
}

#[derive(Deserialize)]
pub struct SendForm {
    phone: String,
    body: String,
}

#[derive(Deserialize)]
pub struct ContactForm {
    phone: String,
    display_name: String,
}

#[derive(Deserialize)]
pub struct TemplateForm {
    phone: String,
    template_name: String,
    language: String,
    parameters_json: String,
}

pub async fn home(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HomeQuery>,
) -> Response {
    if !has_session(&headers, &state.config) {
        return html_response(login_page(None));
    }

    let search = query
        .q
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| truncate(value, 100));
    let conversations = match db::conversations(&state.db, search.as_deref()).await {
        Ok(value) => value,
        Err(_) => return html_response(error_page("Não foi possível ler as conversas locais.")),
    };
    let requested_phone = query.phone.as_deref().and_then(normalize_phone);
    let selected_phone = requested_phone
        .filter(|phone| conversations.iter().any(|item| &item.phone == phone))
        .or_else(|| conversations.first().map(|item| item.phone.clone()));
    let messages = if let Some(phone) = selected_phone.as_deref() {
        match db::messages_for(&state.db, phone).await {
            Ok(value) => value,
            Err(_) => return html_response(error_page("Não foi possível carregar as mensagens.")),
        }
    } else {
        Vec::new()
    };
    let selected_name = selected_phone.as_deref().and_then(|phone| {
        conversations
            .iter()
            .find(|conversation| conversation.phone == phone)
            .and_then(|conversation| conversation.display_name.as_deref())
    });

    html_response(dashboard_page(
        &state.config,
        &conversations,
        &messages,
        selected_phone.as_deref(),
        selected_name,
        search.as_deref(),
        query.sent.is_some(),
        query.template_sent.is_some(),
        query.contact_saved.is_some(),
        query.error.as_deref(),
    ))
}

pub async fn login(State(state): State<AppState>, Form(form): Form<LoginForm>) -> Response {
    let correct = form.password.len() == state.config.app_password.len()
        && bool::from(
            form.password
                .as_bytes()
                .ct_eq(state.config.app_password.as_bytes()),
        );
    if !correct {
        return html_response(login_page(Some("Senha incorreta. Tente novamente.")));
    }

    let token = create_session(&state.config);
    let secure = if state.config.secure_cookie {
        "; Secure"
    } else {
        ""
    };
    let value = format!(
        "crm_session={token}; Path=/; Max-Age={SESSION_SECONDS}; HttpOnly; SameSite=Strict{secure}"
    );
    let mut response = Redirect::to("/").into_response();
    if let Ok(cookie) = value.parse() {
        response.headers_mut().insert(header::SET_COOKIE, cookie);
    }
    response
}

pub async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !has_session(&headers, &state.config) {
        return Redirect::to("/").into_response();
    }
    let secure = if state.config.secure_cookie {
        "; Secure"
    } else {
        ""
    };
    let mut response = Redirect::to("/").into_response();
    if let Ok(cookie) =
        format!("crm_session=; Path=/; Max-Age=0; HttpOnly; SameSite=Strict{secure}").parse()
    {
        response.headers_mut().insert(header::SET_COOKIE, cookie);
    }
    response
}

pub async fn save_contact(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<ContactForm>,
) -> Response {
    if !has_session(&headers, &state.config) {
        return Redirect::to("/").into_response();
    }
    let Some(phone) = normalize_phone(&form.phone) else {
        return Redirect::to("/?error=phone").into_response();
    };
    let display_name = form.display_name.trim();
    if display_name.is_empty() || display_name.chars().count() > 80 {
        return Redirect::to(&format!(
            "/?phone={}&error=contact",
            urlencoding::encode(&phone)
        ))
        .into_response();
    }
    if db::upsert_contact(&state.db, &phone, display_name, "manual")
        .await
        .is_err()
    {
        return html_response(error_page("Não foi possível salvar o contato."));
    }
    Redirect::to(&format!(
        "/?phone={}&contact_saved=1",
        urlencoding::encode(&phone)
    ))
    .into_response()
}

pub async fn send_message(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<SendForm>,
) -> Response {
    if !has_session(&headers, &state.config) {
        return Redirect::to("/").into_response();
    }
    let Some(phone) = normalize_phone(&form.phone) else {
        return Redirect::to("/?error=phone").into_response();
    };
    let body = form.body.trim();
    if body.is_empty() || body.chars().count() > 4096 {
        return redirect_phone(&phone, "text");
    }
    if !state.config.can_send() {
        return redirect_phone(&phone, "config");
    }

    match crate::whatsapp::send_text(&state, &phone, body).await {
        Ok(()) => redirect_phone(&phone, "sent"),
        Err(_) => redirect_phone(&phone, "send"),
    }
}

pub async fn send_template(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<TemplateForm>,
) -> Response {
    if !has_session(&headers, &state.config) {
        return Redirect::to("/").into_response();
    }
    let Some(phone) = normalize_phone(&form.phone) else {
        return Redirect::to("/?error=phone").into_response();
    };
    let name = form.template_name.trim();
    let language = form.language.trim();
    if !valid_template_name(name) || !valid_language_code(language) {
        return redirect_phone(&phone, "template");
    }
    let parameters: Vec<String> = match serde_json::from_str(&form.parameters_json) {
        Ok(parameters) => parameters,
        Err(_) => return redirect_phone(&phone, "template"),
    };
    let parameter_chars = parameters
        .iter()
        .map(|value| value.chars().count())
        .sum::<usize>();
    if parameters.len() > 100
        || parameters.iter().any(|value| value.is_empty())
        || parameter_chars > 4096
    {
        return redirect_phone(&phone, "template");
    }
    if !state.config.can_send() {
        return redirect_phone(&phone, "config");
    }

    match crate::whatsapp::send_template(&state, &phone, name, language, &parameters).await {
        Ok(()) => redirect_phone(&phone, "template_sent"),
        Err(_) => redirect_phone(&phone, "send"),
    }
}

fn redirect_phone(phone: &str, error: &str) -> Response {
    let encoded_phone = urlencoding::encode(phone);
    match error {
        "sent" => Redirect::to(&format!("/?phone={encoded_phone}&sent=1")).into_response(),
        "template_sent" => {
            Redirect::to(&format!("/?phone={encoded_phone}&template_sent=1")).into_response()
        }
        _ => Redirect::to(&format!("/?phone={encoded_phone}&error={error}")).into_response(),
    }
}

fn valid_template_name(name: &str) -> bool {
    (1..=128).contains(&name.len())
        && name
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn valid_language_code(language: &str) -> bool {
    let bytes = language.as_bytes();
    bytes.len() == 5
        && bytes[0..2].iter().all(u8::is_ascii_lowercase)
        && bytes[2] == b'_'
        && bytes[3..5].iter().all(u8::is_ascii_uppercase)
}

fn conversation_href(phone: &str, search: Option<&str>) -> String {
    let phone = urlencoding::encode(phone);
    match search {
        Some(search) => format!("/?phone={phone}&q={}", urlencoding::encode(search)),
        None => format!("/?phone={phone}"),
    }
}

fn login_page(error: Option<&str>) -> Markup {
    html! {
        (DOCTYPE)
        html lang="pt-BR" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { "Entrar · CRM Bling" }
                style { (PreEscaped(CSS)) }
            }
            body class="login-body" {
                main class="login-card" {
                    div class="brand-mark" { "B" }
                    p class="eyebrow" { "CRM BLING · WHATSAPP" }
                    h1 { "Sua operação,\n em um só lugar." }
                    p class="muted" { "Entre para acessar a caixa de entrada da sua empresa." }
                    @if let Some(message) = error {
                        p class="alert alert-error" role="alert" { (message) }
                    }
                    form action="/login" method="post" class="login-form" {
                        label for="password" { "Senha de acesso" }
                        input id="password" type="password" name="password" autocomplete="current-password" required autofocus;
                        button type="submit" class="button button-primary button-wide" { "Entrar no painel" }
                    }
                    p class="login-footnote" { "Acesso privado · Sessão protegida por cookie assinado" }
                }
            }
        }
    }
}

fn dashboard_page(
    config: &Config,
    conversations: &[db::Conversation],
    messages: &[db::Message],
    selected_phone: Option<&str>,
    selected_name: Option<&str>,
    search: Option<&str>,
    sent: bool,
    template_sent: bool,
    contact_saved: bool,
    error: Option<&str>,
) -> Markup {
    let whatsapp_ready = config.can_send();
    let alert = match error {
        Some("phone") => Some(("error", "Informe um número válido com código do país.")),
        Some("contact") => Some(("error", "O nome precisa ter de 1 a 80 caracteres.")),
        Some("text") => Some(("error", "A mensagem precisa ter de 1 a 4096 caracteres.")),
        Some("template") => Some((
            "error",
            "Confira o nome, o idioma e os parâmetros JSON do modelo aprovado.",
        )),
        Some("config") => Some((
            "warning",
            "Configure o token e o ID do número da Cloud API para enviar mensagens.",
        )),
        Some("send") => Some((
            "error",
            "A Meta não confirmou o envio. Confira as credenciais e a janela de atendimento.",
        )),
        _ if sent => Some(("success", "Mensagem enviada e salva na conversa.")),
        _ if template_sent => Some(("success", "Modelo enviado e salvo na conversa.")),
        _ if contact_saved => Some(("success", "Nome do contato salvo.")),
        _ => None,
    };

    html! {
        (DOCTYPE)
        html lang="pt-BR" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { "Caixa de entrada · CRM Bling" }
                style { (PreEscaped(CSS)) }
            }
            body {
                div class="app-shell" {
                    aside class="sidebar" {
                        a href="/" class="brand" aria-label="CRM Bling início" {
                            span class="brand-mark brand-mark-small" { "B" }
                            span { "bling" }
                            span class="brand-dot" { "." }
                        }
                        div class="workspace-label" { "ATENDIMENTO" }
                        div class="inbox-title" {
                            span { "Caixa de entrada" }
                            span class="count-pill" { (conversations.len()) }
                        }
                        div class="conversation-list" {
                            @if conversations.is_empty() {
                                div class="empty-conversations" {
                                    div class="empty-icon" { "↗" }
                                    @if search.is_some() {
                                        p { "Nenhuma conversa encontrada" }
                                        span { "Tente outro nome, telefone ou termo da conversa." }
                                    } @else {
                                        p { "Suas conversas aparecem aqui" }
                                        span { "Quando um cliente enviar uma mensagem, ela será recebida pelo webhook." }
                                    }
                                }
                            } @else {
                                @for conversation in conversations {
                                    @let conversation_class = if selected_phone == Some(conversation.phone.as_str()) {
                                        "conversation conversation-active"
                                    } else {
                                        "conversation"
                                    };
                                    a href=(conversation_href(&conversation.phone, search))
                                        class=(conversation_class) {
                                        span class="avatar" { (avatar_letters(&conversation.phone, conversation.display_name.as_deref())) }
                                        span class="conversation-copy" {
                                            strong { (conversation.display_name.as_deref().unwrap_or(&conversation.phone)) }
                                            span class="preview" { (truncate(&conversation.preview, 58)) }
                                        }
                                        span class="conversation-side" {
                                            time { (format_time(conversation.latest_at)) }
                                            span class="mini-count" { (conversation.message_count) }
                                        }
                                    }
                                }
                            }
                        }
                        div class="sidebar-bottom" {
                            span class=(if whatsapp_ready { "connection-dot online" } else { "connection-dot offline" }) {}
                            span { @if whatsapp_ready { "Credenciais preenchidas" } @else { "Credenciais pendentes" } }
                            form action="/logout" method="post" class="logout-form" {
                                button type="submit" class="logout-button" title="Sair" aria-label="Sair" { "↪" }
                            }
                        }
                    }

                    main class="main-panel" {
                        header class="topbar" {
                            div class="breadcrumb" { "Atendimento" span { "/" } strong { "Conversas" } }
                            form action="/" method="get" class="conversation-search" role="search" {
                                label for="conversation-search" class="sr-only" { "Buscar conversas" }
                                input id="conversation-search" name="q" type="search" maxlength="100"
                                    value=[search] placeholder="Nome, telefone ou mensagem" autocomplete="off";
                                button type="submit" aria-label="Buscar conversas" { "⌕" }
                            }
                            div class="topbar-right" {
                                span class="api-status" {
                                    span class=(if whatsapp_ready { "connection-dot online" } else { "connection-dot offline" }) {}
                                    @if whatsapp_ready { "Credenciais configuradas" } @else { "Configure as credenciais" }
                                }
                                span class="profile-avatar" { "AD" }
                            }
                        }

                        @if let Some((kind, message)) = alert {
                                    div class=(format!("alert app-alert alert-{kind}")) role="status" { (message) }
                        }

                        @if let Some(phone) = selected_phone {
                            section class="chat-panel" {
                                header class="chat-header" {
                                    span class="avatar avatar-large" { (avatar_letters(phone, selected_name)) }
                                    div class="chat-person" {
                                        h1 {
                                            @if let Some(name) = selected_name { (name) }
                                            @else { (format_phone(phone)) }
                                        }
                                        span { (format_phone(phone)) " · WhatsApp" }
                                        form action="/contacts" method="post" class="contact-editor" {
                                            input type="hidden" name="phone" value=(phone);
                                            label for="contact-name" class="sr-only" { "Nome do contato" }
                                            input id="contact-name" name="display_name" type="text" maxlength="80"
                                                required value=[selected_name] placeholder="Salvar nome do contato" autocomplete="name";
                                            button type="submit" { "Salvar nome" }
                                        }
                                    }
                                    span class="chat-header-tag" { "WHATSAPP" }
                                }

                                div class="messages" id="messages" {
                                    @if messages.is_empty() {
                                        div class="first-message" {
                                            span { "✦" }
                                            strong { "Inicie a conversa" }
                                            p { "Envie uma mensagem para este número. Confirme que há consentimento do contato e respeite as regras de atendimento da Meta." }
                                        }
                                    }
                                    @for message in messages {
                                        article class=(if message.direction == "outbound" { "message message-outbound" } else { "message message-inbound" }) {
                                            p { (message.body) }
                                            div class="message-meta" {
                                                time { (format_time(message.created_at)) }
                                                @if message.direction == "outbound" {
                                                    span { " · " (status_label(&message.status)) }
                                                }
                                            }
                                        }
                                    }
                                }

                                form action="/messages" method="post" class="composer" {
                                    label for="phone" class="composer-label" { "Número com DDI" }
                                    div class="phone-input-wrap" {
                                        span { "+" }
                                        input id="phone" name="phone" type="tel" inputmode="tel" required value=(phone)
                                            placeholder="5511999999999" autocomplete="off";
                                    }
                                    label for="body" class="sr-only" { "Mensagem" }
                                    textarea id="body" name="body" rows="3" maxlength="4096" required
                                        placeholder="Escreva sua mensagem..." {}
                                    div class="composer-footer" {
                                        span class="message-rule" { "Texto · até 4096 caracteres" }
                                        button type="submit" class="button button-primary send-button" disabled[!whatsapp_ready] {
                                            "Enviar" span { "↑" }
                                        }
                                    }
                                }
                                details class="template-composer" {
                                    summary { "Enviar modelo aprovado" }
                                    form action="/messages/template" method="post" {
                                        input type="hidden" name="phone" value=(phone);
                                        div class="template-fields" {
                                            div {
                                                label for="template-name" { "Nome do modelo" }
                                                input id="template-name" name="template_name" type="text" required
                                                    maxlength="128" pattern="[a-z][a-z0-9_]*" placeholder="confirmacao_pedido";
                                            }
                                            div {
                                                label for="template-language" { "Idioma aprovado" }
                                                input id="template-language" name="language" type="text" required
                                                    pattern="[a-z]{2}_[A-Z]{2}" value="pt_BR" placeholder="pt_BR";
                                            }
                                        }
                                        label for="template-parameters" { "Parâmetros do corpo · array JSON" }
                                        textarea id="template-parameters" name="parameters_json" rows="2" required
                                            maxlength="8192" { "[]" }
                                        div class="composer-footer" {
                                            span class="message-rule" { "Use os valores na ordem dos campos {{1}}, {{2}}…" }
                                            button type="submit" class="button button-primary send-button" disabled[!whatsapp_ready] {
                                                "Enviar modelo" span { "↑" }
                                            }
                                        }
                                    }
                                }
                            }
                        } @else if search.is_some() {
                            section class="welcome-panel search-empty-state" {
                                div class="welcome-icon" { "⌕" }
                                p class="eyebrow" { "BUSCA DE CONVERSAS" }
                                h1 { "Nenhum resultado encontrado." }
                                p { "Não encontramos conversas com esse nome, telefone ou mensagem." }
                                a href="/" class="button button-primary" { "Limpar busca" }
                            }
                        } @else {
                            section class="welcome-panel" {
                                div class="welcome-icon" { "✦" }
                                p class="eyebrow" { "RELACIONAMENTO EM TEMPO REAL" }
                                h1 { "Converse com seus clientes." }
                                p { "As mensagens recebidas aparecerão nesta caixa. Configure o webhook da Meta para começar a receber conversas." }
                                form action="/messages" method="post" class="welcome-composer" {
                                    label for="new-phone" class="composer-label" { "Novo atendimento · número com DDI" }
                                    input id="new-phone" name="phone" type="tel" inputmode="tel" required
                                        placeholder="5511999999999" autocomplete="off";
                                    label for="new-body" class="sr-only" { "Mensagem inicial" }
                                    textarea id="new-body" name="body" rows="3" maxlength="4096" required
                                        placeholder="Escreva a primeira mensagem..." {}
                                    div class="composer-footer" {
                                        span class="message-rule" { "Envie somente para contatos com opt-in." }
                                        button type="submit" class="button button-primary send-button" disabled[!whatsapp_ready] {
                                            "Enviar" span { "↑" }
                                        }
                                    }
                                }
                                @if !whatsapp_ready {
                                    div class="setup-callout" {
                                        strong { "Próximo passo" }
                                        span { "Adicione as credenciais da WhatsApp Cloud API nas variáveis de ambiente. Consulte o README." }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn error_page(message: &str) -> Markup {
    html! {
        (DOCTYPE)
        html lang="pt-BR" {
            head { meta charset="utf-8"; meta name="viewport" content="width=device-width, initial-scale=1"; title { "Erro · CRM Bling" } style { (PreEscaped(CSS)) } }
            body class="login-body" { main class="login-card" { p class="eyebrow" { "CRM BLING" } h1 { "Não foi possível abrir o painel." } p class="muted" { (message) } a href="/" class="button button-primary" { "Voltar" } } }
        }
    }
}

fn has_session(headers: &HeaderMap, config: &Config) -> bool {
    let Some(cookie_header) = headers
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let Some(token) = cookie_header.split(';').find_map(|cookie| {
        let (name, value) = cookie.trim().split_once('=')?;
        (name == "crm_session").then_some(value)
    }) else {
        return false;
    };
    let Some((payload, signature)) = token.split_once('.') else {
        return false;
    };
    let Ok(signature) = URL_SAFE_NO_PAD.decode(signature) else {
        return false;
    };
    let Ok(mut mac) = HmacSha256::new_from_slice(&config.session_secret) else {
        return false;
    };
    mac.update(payload.as_bytes());
    if mac.verify_slice(&signature).is_err() {
        return false;
    }
    let Ok(claims) = URL_SAFE_NO_PAD.decode(payload) else {
        return false;
    };
    let Ok(claims) = String::from_utf8(claims) else {
        return false;
    };
    let Some((expires, _nonce)) = claims.split_once('.') else {
        return false;
    };
    expires
        .parse::<i64>()
        .is_ok_and(|expires| expires > Utc::now().timestamp())
}

fn create_session(config: &Config) -> String {
    let expires = Utc::now().timestamp() + SESSION_SECONDS;
    let claims = format!("{expires}.{}", Uuid::new_v4().simple());
    let payload = URL_SAFE_NO_PAD.encode(claims);
    let mut mac = HmacSha256::new_from_slice(&config.session_secret)
        .expect("HMAC accepts secrets of any length");
    mac.update(payload.as_bytes());
    let signature = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
    format!("{payload}.{signature}")
}

fn normalize_phone(input: &str) -> Option<String> {
    let input = input.trim();
    if input.chars().filter(|character| *character == '+').count() > 1
        || input.find('+').is_some_and(|position| position != 0)
    {
        return None;
    }
    let mut digits = String::with_capacity(input.len());
    for character in input.chars() {
        if character.is_ascii_digit() {
            digits.push(character);
        } else if !matches!(character, '+' | ' ' | '-' | '(' | ')' | '.') {
            return None;
        }
    }
    (8..=15).contains(&digits.len()).then_some(digits)
}

fn format_phone(phone: &str) -> String {
    format!("+{phone}")
}

fn avatar_letters(phone: &str, display_name: Option<&str>) -> String {
    if let Some(name) = display_name {
        let mut initials = String::new();
        for part in name.split_whitespace().take(2) {
            if let Some(character) = part.chars().next() {
                initials.extend(character.to_uppercase());
            }
        }
        if !initials.is_empty() {
            return initials;
        }
    }
    phone
        .chars()
        .rev()
        .take(2)
        .collect::<String>()
        .chars()
        .rev()
        .collect()
}

fn truncate(value: &str, max: usize) -> String {
    let mut chars = value.chars();
    let prefix: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

fn format_time(timestamp: i64) -> String {
    Utc.timestamp_opt(timestamp, 0)
        .single()
        .map(|value| value.format("%d/%m %H:%M").to_string())
        .unwrap_or_else(|| "—".into())
}

fn status_label(status: &str) -> &str {
    match status {
        "sent" => "enviada",
        "delivered" => "entregue",
        "read" => "lida",
        "failed" => "falhou",
        value => value,
    }
}

fn html_response(markup: Markup) -> Response {
    axum::response::Html(markup.into_string()).into_response()
}
