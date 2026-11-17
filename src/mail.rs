use std::{env::consts::EXE_SUFFIX, str::FromStr};

use chrono::NaiveDateTime;
use lettre::{
    Address, AsyncSendmailTransport, AsyncSmtpTransport, AsyncTransport, Tokio1Executor,
    message::{Attachment, Body, Mailbox, Message, MultiPart, SinglePart},
    transport::smtp::authentication::{Credentials, Mechanism as SmtpAuthMechanism},
    transport::smtp::client::{Tls, TlsParameters},
    transport::smtp::extension::ClientId,
};
use percent_encoding::{NON_ALPHANUMERIC, percent_encode};

use crate::{
    CONFIG,
    api::EmptyResult,
    auth::{
        encode_jwt, generate_delete_claims, generate_emergency_access_invite_claims, generate_invite_claims,
        generate_verify_email_claims,
    },
    db::models::{Device, DeviceType, EmergencyAccessId, MembershipId, OrganizationId, User, UserId},
    error::Error,
    locale::{Locale, locale_for_address, localized_template_name},
    util::upcase_first,
};

fn sendmail_transport() -> AsyncSendmailTransport<Tokio1Executor> {
    if let Some(command) = CONFIG.sendmail_command() {
        AsyncSendmailTransport::new_with_command(command)
    } else {
        AsyncSendmailTransport::new_with_command(format!("sendmail{EXE_SUFFIX}"))
    }
}

fn smtp_transport() -> AsyncSmtpTransport<Tokio1Executor> {
    use std::time::Duration;
    let host = CONFIG.smtp_host().unwrap();

    let smtp_client = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(host.as_str())
        .port(CONFIG.smtp_port())
        .timeout(Some(Duration::from_secs(u64::from(CONFIG.smtp_timeout()))));

    // Determine security
    let smtp_client = if CONFIG.smtp_security() == *"off" {
        smtp_client
    } else {
        let mut tls_parameters = TlsParameters::builder(host);
        if CONFIG.smtp_accept_invalid_hostnames() {
            tls_parameters = tls_parameters.dangerous_accept_invalid_hostnames(true);
        }
        if CONFIG.smtp_accept_invalid_certs() {
            tls_parameters = tls_parameters.dangerous_accept_invalid_certs(true);
        }
        let tls_parameters = tls_parameters.build().unwrap();

        if CONFIG.smtp_security() == *"force_tls" {
            smtp_client.tls(Tls::Wrapper(tls_parameters))
        } else {
            smtp_client.tls(Tls::Required(tls_parameters))
        }
    };

    let smtp_client = match (CONFIG.smtp_username(), CONFIG.smtp_password()) {
        (Some(user), Some(pass)) => smtp_client.credentials(Credentials::new(user, pass)),
        _ => smtp_client,
    };

    let smtp_client = match CONFIG.helo_name() {
        Some(helo_name) => smtp_client.hello_name(ClientId::Domain(helo_name)),
        None => smtp_client,
    };

    let smtp_client = match CONFIG.smtp_auth_mechanism() {
        Some(mechanism) => {
            let allowed_mechanisms = [SmtpAuthMechanism::Plain, SmtpAuthMechanism::Login, SmtpAuthMechanism::Xoauth2];
            let mut selected_mechanisms = vec![];
            for wanted_mechanism in mechanism.split(',') {
                for m in &allowed_mechanisms {
                    if m.to_string().to_lowercase()
                        == wanted_mechanism.trim_matches(|c| c == '"' || c == '\'' || c == ' ').to_lowercase()
                    {
                        selected_mechanisms.push(*m);
                    }
                }
            }

            if selected_mechanisms.is_empty() {
                // Only show a warning, and return without setting an actual authentication mechanism
                warn!("No valid SMTP Auth mechanism found for '{mechanism}', using default values");
                smtp_client
            } else {
                smtp_client.authentication(selected_mechanisms)
            }
        }
        _ => smtp_client,
    };

    smtp_client.build()
}

// This will sanitize the string values by stripping all the html tags to prevent XSS and HTML Injections
fn sanitize_data(data: &mut serde_json::Value) {
    use regex::Regex;
    use std::sync::LazyLock;
    static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]+>").unwrap());

    match data {
        serde_json::Value::String(s) => *s = RE.replace_all(s, "").to_string(),
        serde_json::Value::Object(obj) => {
            for d in obj.values_mut() {
                sanitize_data(d);
            }
        }
        serde_json::Value::Array(arr) => {
            for d in arr.iter_mut() {
                sanitize_data(d);
            }
        }
        _ => {}
    }
}

async fn get_text(
    template_name: &'static str,
    address: &str,
    data: serde_json::Value,
) -> Result<(String, String, String), Error> {
    let mut data = data;
    sanitize_data(&mut data);
    render_text(locale_for_address(address).await, template_name, &data)
}

// An email is never lost because of a translation: any failure of the localized template falls back to English.
fn render_text(
    locale: Locale,
    template_name: &str,
    data: &serde_json::Value,
) -> Result<(String, String, String), Error> {
    let localized_name = localized_template_name(locale, template_name);
    if localized_name != template_name {
        match render_text(Locale::En, &localized_name, data) {
            Ok(text) => return Ok(text),
            Err(e) => warn!("Cannot render the template {localized_name}, using {template_name}: {e:?}"),
        }
    }
    let (subject_html, body_html) = get_template(&format!("{template_name}.html"), data)?;
    let (_subject_text, body_text) = get_template(template_name, data)?;
    Ok((subject_html, body_html, body_text))
}

fn get_template(template_name: &str, data: &serde_json::Value) -> Result<(String, String), Error> {
    let text = CONFIG.render_template(template_name, data)?;
    let mut text_split = text.split("<!---------------->");

    let subject = if let Some(s) = text_split.next() {
        s.trim().to_owned()
    } else {
        err!("Template doesn't contain subject")
    };

    let body = if let Some(s) = text_split.next() {
        s.trim().to_owned()
    } else {
        err!("Template doesn't contain body")
    };

    if text_split.next().is_some() {
        err!("Template contains more than one body");
    }

    Ok((subject, body))
}

pub async fn send_password_hint(address: &str, hint: Option<String>) -> EmptyResult {
    let template_name = if hint.is_some() {
        "email/pw_hint_some"
    } else {
        "email/pw_hint_none"
    };

    let (subject, body_html, body_text) = get_text(
        template_name,
        address,
        json!({
            "url": CONFIG.domain(),
            "img_src": CONFIG._smtp_img_src(),
            "hint": hint,
        }),
    )
    .await?;

    send_email(address, &subject, body_html, body_text).await
}

pub async fn send_delete_account(address: &str, user_id: &UserId) -> EmptyResult {
    let claims = generate_delete_claims(user_id.to_string())?;
    let delete_token = encode_jwt(&claims);

    let (subject, body_html, body_text) = get_text(
        "email/delete_account",
        address,
        json!({
            "url": CONFIG.domain(),
            "img_src": CONFIG._smtp_img_src(),
            "user_id": user_id,
            "email": percent_encode(address.as_bytes(), NON_ALPHANUMERIC).to_string(),
            "token": delete_token,
        }),
    )
    .await?;

    send_email(address, &subject, body_html, body_text).await
}

pub async fn send_verify_email(address: &str, user_id: &UserId) -> EmptyResult {
    let claims = generate_verify_email_claims(user_id)?;
    let verify_email_token = encode_jwt(&claims);

    let (subject, body_html, body_text) = get_text(
        "email/verify_email",
        address,
        json!({
            "url": CONFIG.domain(),
            "img_src": CONFIG._smtp_img_src(),
            "user_id": user_id,
            "email": percent_encode(address.as_bytes(), NON_ALPHANUMERIC).to_string(),
            "token": verify_email_token,
        }),
    )
    .await?;

    send_email(address, &subject, body_html, body_text).await
}

pub async fn send_register_verify_email(email: &str, token: &str) -> EmptyResult {
    let mut query = url::Url::parse("https://query.builder").unwrap();
    query.query_pairs_mut().append_pair("email", email).append_pair("token", token).append_pair("fromEmail", "true");
    let Some(query_string) = query.query() else {
        err!("Failed to build verify URL query parameters")
    };

    let (subject, body_html, body_text) = get_text(
        "email/register_verify_email",
        email,
        json!({
            // `url.Url` would place the anchor `#` after the query parameters
            "url": format!("{}/#/finish-signup/?{query_string}", CONFIG.domain()),
            "img_src": CONFIG._smtp_img_src(),
            "email": email,
        }),
    )
    .await?;

    send_email(email, &subject, body_html, body_text).await
}

pub async fn send_welcome(address: &str) -> EmptyResult {
    let (subject, body_html, body_text) = get_text(
        "email/welcome",
        address,
        json!({
            "url": CONFIG.domain(),
            "img_src": CONFIG._smtp_img_src(),
        }),
    )
    .await?;

    send_email(address, &subject, body_html, body_text).await
}

pub async fn send_welcome_must_verify(address: &str, user_id: &UserId) -> EmptyResult {
    let claims = generate_verify_email_claims(user_id)?;
    let verify_email_token = encode_jwt(&claims);

    let (subject, body_html, body_text) = get_text(
        "email/welcome_must_verify",
        address,
        json!({
            "url": CONFIG.domain(),
            "img_src": CONFIG._smtp_img_src(),
            "user_id": user_id,
            "token": verify_email_token,
        }),
    )
    .await?;

    send_email(address, &subject, body_html, body_text).await
}

pub async fn send_2fa_removed_from_org(address: &str, org_name: &str) -> EmptyResult {
    let (subject, body_html, body_text) = get_text(
        "email/send_2fa_removed_from_org",
        address,
        json!({
            "url": CONFIG.domain(),
            "img_src": CONFIG._smtp_img_src(),
            "org_name": org_name,
        }),
    )
    .await?;

    send_email(address, &subject, body_html, body_text).await
}

pub async fn send_single_org_removed_from_org(address: &str, org_name: &str) -> EmptyResult {
    let (subject, body_html, body_text) = get_text(
        "email/send_single_org_removed_from_org",
        address,
        json!({
            "url": CONFIG.domain(),
            "img_src": CONFIG._smtp_img_src(),
            "org_name": org_name,
        }),
    )
    .await?;

    send_email(address, &subject, body_html, body_text).await
}

pub async fn send_invite(
    user: &User,
    org_id: OrganizationId,
    member_id: MembershipId,
    org_name: &str,
    invited_by_email: Option<String>,
) -> EmptyResult {
    let claims = generate_invite_claims(
        user.uuid.clone(),
        user.email.clone(),
        org_id.clone(),
        member_id.clone(),
        invited_by_email,
    )?;
    let invite_token = encode_jwt(&claims);
    let mut query = url::Url::parse("https://query.builder").unwrap();
    {
        let mut query_params = query.query_pairs_mut();
        query_params
            .append_pair("email", &user.email)
            .append_pair("organizationName", org_name)
            .append_pair("organizationId", &org_id)
            .append_pair("organizationUserId", &member_id)
            .append_pair("token", &invite_token);

        if CONFIG.sso_enabled() && CONFIG.sso_only() {
            query_params.append_pair("orgSsoIdentifier", &org_id);
        }

        // The web vault requires both of these parameters to be present.
        // If either is missing it rejects the invite client-side, before any
        // request reaches the server, showing only "Unable to accept invitation".
        query_params.append_pair("initOrganization", "false");

        let org_user_has_existing_user = if user.private_key.is_some() {
            "true"
        } else {
            "false"
        };
        query_params.append_pair("orgUserHasExistingUser", org_user_has_existing_user);
    }

    let Some(query_string) = query.query() else {
        err!("Failed to build invite URL query parameters")
    };

    let (subject, body_html, body_text) = get_text(
        "email/send_org_invite",
        &user.email,
        json!({
            // `url.Url` would place the anchor `#` after the query parameters
            "url": format!("{}/#/accept-organization/?{query_string}", CONFIG.domain()),
            "img_src": CONFIG._smtp_img_src(),
            "org_name": org_name,
        }),
    )
    .await?;

    send_email(&user.email, &subject, body_html, body_text).await
}

pub async fn send_emergency_access_invite(
    address: &str,
    user_id: UserId,
    emer_id: EmergencyAccessId,
    grantor_name: &str,
    grantor_email: &str,
) -> EmptyResult {
    let claims = generate_emergency_access_invite_claims(
        user_id,
        String::from(address),
        emer_id.clone(),
        String::from(grantor_name),
        String::from(grantor_email),
    )?;

    // Build the query here to ensure proper escaping
    let mut query = url::Url::parse("https://query.builder").unwrap();
    {
        let mut query_params = query.query_pairs_mut();
        query_params
            .append_pair("id", &emer_id.to_string())
            .append_pair("name", grantor_name)
            .append_pair("email", address)
            .append_pair("token", &encode_jwt(&claims));
    }

    let Some(query_string) = query.query() else {
        err!("Failed to build emergency invite URL query parameters")
    };

    let (subject, body_html, body_text) = get_text(
        "email/send_emergency_access_invite",
        address,
        json!({
            // `url.Url` would place the anchor `#` after the query parameters
            "url": format!("{}/#/accept-emergency/?{query_string}", CONFIG.domain()),
            "img_src": CONFIG._smtp_img_src(),
            "grantor_name": grantor_name,
        }),
    )
    .await?;

    send_email(address, &subject, body_html, body_text).await
}

pub async fn send_emergency_access_invite_accepted(address: &str, grantee_email: &str) -> EmptyResult {
    let (subject, body_html, body_text) = get_text(
        "email/emergency_access_invite_accepted",
        address,
        json!({
            "url": CONFIG.domain(),
            "img_src": CONFIG._smtp_img_src(),
            "grantee_email": grantee_email,
        }),
    )
    .await?;

    send_email(address, &subject, body_html, body_text).await
}

pub async fn send_emergency_access_invite_confirmed(address: &str, grantor_name: &str) -> EmptyResult {
    let (subject, body_html, body_text) = get_text(
        "email/emergency_access_invite_confirmed",
        address,
        json!({
            "url": CONFIG.domain(),
            "img_src": CONFIG._smtp_img_src(),
            "grantor_name": grantor_name,
        }),
    )
    .await?;

    send_email(address, &subject, body_html, body_text).await
}

pub async fn send_emergency_access_recovery_approved(address: &str, grantor_name: &str) -> EmptyResult {
    let (subject, body_html, body_text) = get_text(
        "email/emergency_access_recovery_approved",
        address,
        json!({
            "url": CONFIG.domain(),
            "img_src": CONFIG._smtp_img_src(),
            "grantor_name": grantor_name,
        }),
    )
    .await?;

    send_email(address, &subject, body_html, body_text).await
}

pub async fn send_emergency_access_recovery_initiated(
    address: &str,
    grantee_name: &str,
    atype: &str,
    wait_time_days: &i32,
) -> EmptyResult {
    let (subject, body_html, body_text) = get_text(
        "email/emergency_access_recovery_initiated",
        address,
        json!({
            "url": CONFIG.domain(),
            "img_src": CONFIG._smtp_img_src(),
            "grantee_name": grantee_name,
            "atype": atype,
            "wait_time_days": wait_time_days,
        }),
    )
    .await?;

    send_email(address, &subject, body_html, body_text).await
}

pub async fn send_emergency_access_recovery_reminder(
    address: &str,
    grantee_name: &str,
    atype: &str,
    days_left: &str,
) -> EmptyResult {
    let (subject, body_html, body_text) = get_text(
        "email/emergency_access_recovery_reminder",
        address,
        json!({
            "url": CONFIG.domain(),
            "img_src": CONFIG._smtp_img_src(),
            "grantee_name": grantee_name,
            "atype": atype,
            "days_left": days_left,
        }),
    )
    .await?;

    send_email(address, &subject, body_html, body_text).await
}

pub async fn send_emergency_access_recovery_rejected(address: &str, grantor_name: &str) -> EmptyResult {
    let (subject, body_html, body_text) = get_text(
        "email/emergency_access_recovery_rejected",
        address,
        json!({
            "url": CONFIG.domain(),
            "img_src": CONFIG._smtp_img_src(),
            "grantor_name": grantor_name,
        }),
    )
    .await?;

    send_email(address, &subject, body_html, body_text).await
}

pub async fn send_emergency_access_recovery_timed_out(address: &str, grantee_name: &str, atype: &str) -> EmptyResult {
    let (subject, body_html, body_text) = get_text(
        "email/emergency_access_recovery_timed_out",
        address,
        json!({
            "url": CONFIG.domain(),
            "img_src": CONFIG._smtp_img_src(),
            "grantee_name": grantee_name,
            "atype": atype,
        }),
    )
    .await?;

    send_email(address, &subject, body_html, body_text).await
}

pub async fn send_invite_accepted(new_user_email: &str, address: &str, org_name: &str) -> EmptyResult {
    let (subject, body_html, body_text) = get_text(
        "email/invite_accepted",
        address,
        json!({
            "url": CONFIG.domain(),
            "img_src": CONFIG._smtp_img_src(),
            "email": new_user_email,
            "org_name": org_name,
        }),
    )
    .await?;

    send_email(address, &subject, body_html, body_text).await
}

pub async fn send_invite_confirmed(address: &str, org_name: &str) -> EmptyResult {
    let (subject, body_html, body_text) = get_text(
        "email/invite_confirmed",
        address,
        json!({
            "url": CONFIG.domain(),
            "img_src": CONFIG._smtp_img_src(),
            "org_name": org_name,
        }),
    )
    .await?;

    send_email(address, &subject, body_html, body_text).await
}

pub async fn send_new_device_logged_in(address: &str, ip: &str, dt: &NaiveDateTime, device: &Device) -> EmptyResult {
    let fmt = "%A, %B %_d, %Y at %r %Z";
    let (subject, body_html, body_text) = get_text(
        "email/new_device_logged_in",
        address,
        json!({
            "url": CONFIG.domain(),
            "img_src": CONFIG._smtp_img_src(),
            "ip": ip,
            "device_name": upcase_first(&device.name),
            "device_type": DeviceType::from_i32(device.atype).to_string(),
            "datetime": crate::util::format_naive_datetime_local(dt, fmt),
        }),
    )
    .await?;

    send_email(address, &subject, body_html, body_text).await
}

pub async fn send_recover_twofactor(address: &str, ip: &str, dt: &NaiveDateTime) -> EmptyResult {
    let fmt = "%A, %B %_d, %Y at %r %Z";
    let (subject, body_html, body_text) = get_text(
        "email/recover_twofactor",
        address,
        json!({
            "url": CONFIG.domain(),
            "img_src": CONFIG._smtp_img_src(),
            "ip": ip,
            "datetime": crate::util::format_naive_datetime_local(dt, fmt),
        }),
    )
    .await?;

    send_email(address, &subject, body_html, body_text).await
}

pub async fn send_incomplete_2fa_login(
    address: &str,
    ip: &str,
    dt: &NaiveDateTime,
    device_name: &str,
    device_type: &str,
) -> EmptyResult {
    let fmt = "%A, %B %_d, %Y at %r %Z";
    let (subject, body_html, body_text) = get_text(
        "email/incomplete_2fa_login",
        address,
        json!({
            "url": CONFIG.domain(),
            "img_src": CONFIG._smtp_img_src(),
            "ip": ip,
            "device_name": upcase_first(device_name),
            "device_type": device_type,
            "datetime": crate::util::format_naive_datetime_local(dt, fmt),
            "time_limit": CONFIG.incomplete_2fa_time_limit(),
        }),
    )
    .await?;

    send_email(address, &subject, body_html, body_text).await
}

pub async fn send_token(address: &str, token: &str) -> EmptyResult {
    let (subject, body_html, body_text) = get_text(
        "email/twofactor_email",
        address,
        json!({
            "url": CONFIG.domain(),
            "img_src": CONFIG._smtp_img_src(),
            "token": token,
        }),
    )
    .await?;

    send_email(address, &subject, body_html, body_text).await
}

pub async fn send_change_email(address: &str, token: &str) -> EmptyResult {
    let (subject, body_html, body_text) = get_text(
        "email/change_email",
        address,
        json!({
            "url": CONFIG.domain(),
            "img_src": CONFIG._smtp_img_src(),
            "token": token,
        }),
    )
    .await?;

    send_email(address, &subject, body_html, body_text).await
}

pub async fn send_change_email_existing(address: &str, acting_address: &str) -> EmptyResult {
    let (subject, body_html, body_text) = get_text(
        "email/change_email_existing",
        address,
        json!({
            "url": CONFIG.domain(),
            "img_src": CONFIG._smtp_img_src(),
            "existing_address": address,
            "acting_address": acting_address,
        }),
    )
    .await?;

    send_email(address, &subject, body_html, body_text).await
}

pub async fn send_change_email_invited(address: &str, acting_address: &str) -> EmptyResult {
    let (subject, body_html, body_text) = get_text(
        "email/change_email_invited",
        address,
        json!({
            "url": CONFIG.domain(),
            "img_src": CONFIG._smtp_img_src(),
            "existing_address": address,
            "acting_address": acting_address,
        }),
    )
    .await?;

    send_email(address, &subject, body_html, body_text).await
}

pub async fn send_sso_change_email(address: &str) -> EmptyResult {
    let (subject, body_html, body_text) = get_text(
        "email/sso_change_email",
        address,
        json!({
            "url": format!("{}/#/settings/account", CONFIG.domain()),
            "img_src": CONFIG._smtp_img_src(),
        }),
    )
    .await?;

    send_email(address, &subject, body_html, body_text).await
}

pub async fn send_test(address: &str) -> EmptyResult {
    let (subject, body_html, body_text) = get_text(
        "email/smtp_test",
        address,
        json!({
            "url": CONFIG.domain(),
            "img_src": CONFIG._smtp_img_src(),
        }),
    )
    .await?;

    send_email(address, &subject, body_html, body_text).await
}

pub async fn send_admin_account_recovery(
    address: &str,
    user_name: &str,
    org_name: &str,
    reset_password: bool,
    reset_2fa: bool,
    fallback_2fa_email: bool,
) -> EmptyResult {
    let (subject, body_html, body_text) = get_text(
        "email/admin_account_recovery",
        address,
        json!({
            "url": CONFIG.domain(),
            "img_src": CONFIG._smtp_img_src(),
            "user_name": user_name,
            "org_name": org_name,
            "reset_password": reset_password,
            "reset_2fa": reset_2fa,
            "fallback_2fa_email": fallback_2fa_email,
        }),
    )
    .await?;
    send_email(address, &subject, body_html, body_text).await
}

pub async fn send_protected_action_token(address: &str, token: &str) -> EmptyResult {
    let (subject, body_html, body_text) = get_text(
        "email/protected_action",
        address,
        json!({
            "url": CONFIG.domain(),
            "img_src": CONFIG._smtp_img_src(),
            "token": token,
        }),
    )
    .await?;

    send_email(address, &subject, body_html, body_text).await
}

async fn send_with_selected_transport(email: Message) -> EmptyResult {
    if CONFIG.use_sendmail() {
        match sendmail_transport().send(email).await {
            Ok(()) => Ok(()),
            // Match some common errors and make them more user friendly
            Err(e) => {
                if e.is_client() {
                    debug!("Sendmail client error: {e:?}");
                    err!(format!("Sendmail client error: {e}"));
                } else if e.is_response() {
                    debug!("Sendmail response error: {e:?}");
                    err!(format!("Sendmail response error: {e}"));
                }
                debug!("Sendmail error: {e:?}");
                err!(format!("Sendmail error: {e}"));
            }
        }
    } else {
        match smtp_transport().send(email).await {
            Ok(_) => Ok(()),
            // Match some common errors and make them more user friendly
            Err(e) => {
                if e.is_client() {
                    debug!("SMTP client error: {e:#?}");
                    err!(format!("SMTP client error: {e}"));
                } else if e.is_transient() {
                    debug!("SMTP 4xx error: {e:#?}");
                    err!(format!("SMTP 4xx error: {e}"));
                } else if e.is_permanent() {
                    debug!("SMTP 5xx error: {e:#?}");
                    let mut msg = e.to_string();
                    // Add a special check for 535 to add a more descriptive message
                    if msg.contains("(535)") {
                        msg = format!("{msg} - Authentication credentials invalid");
                    }
                    err!(format!("SMTP 5xx error: {msg}"));
                } else if e.is_timeout() {
                    debug!("SMTP timeout error: {e:#?}");
                    err!(format!("SMTP timeout error: {e}"));
                } else if e.is_tls() {
                    debug!("SMTP encryption error: {e:#?}");
                    err!(format!("SMTP encryption error: {e}"));
                }
                debug!("SMTP error: {e:#?}");
                err!(format!("SMTP error: {e}"));
            }
        }
    }
}

async fn send_email(address: &str, subject: &str, body_html: String, body_text: String) -> EmptyResult {
    let smtp_from = Address::from_str(&CONFIG.smtp_from())?;

    let body = if CONFIG.smtp_embed_images() {
        let logo_gray_body = Body::new(crate::api::STATIC_FILES["logo-gray.png"].bytes.to_vec());
        let mail_github_body = Body::new(crate::api::STATIC_FILES["mail-github.png"].bytes.to_vec());
        MultiPart::alternative().singlepart(SinglePart::plain(body_text)).multipart(
            MultiPart::related()
                .singlepart(SinglePart::html(body_html))
                .singlepart(
                    Attachment::new_inline(String::from("logo-gray.png"))
                        .body(logo_gray_body, "image/png".parse().unwrap()),
                )
                .singlepart(
                    Attachment::new_inline(String::from("mail-github.png"))
                        .body(mail_github_body, "image/png".parse().unwrap()),
                ),
        )
    } else {
        MultiPart::alternative_plain_html(body_text, body_html)
    };

    let email = Message::builder()
        .message_id(Some(format!("<{}@{}>", crate::util::get_uuid(), smtp_from.domain())))
        .to(Mailbox::new(None, Address::from_str(address)?))
        .from(Mailbox::new(Some(CONFIG.smtp_from_name()), smtp_from))
        .subject(subject)
        .multipart(body)?;

    send_with_selected_transport(email).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::locale::in_isolated_config;

    const EMAIL_TEMPLATES_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/static/templates/email");
    const SHARED_PARTIALS: [&str; 3] = ["email_header", "email_footer", "email_footer_text"];
    const BOOLEAN_VARS: [&str; 3] = ["fallback_2fa_email", "reset_2fa", "reset_password"];
    const WELCOME: &str = "email/welcome";
    const ONLY_ENGLISH: &str = "email/only_english";
    const TEMPLATE_BODY_SEPARATOR: &str = "<!---------------->";
    const SAMPLE_OVERRIDE_SUBJECT: &str = "Sample override subject";

    /// The variables each email template reads, as the `send_*` functions pass them (`img_src` is added to all).
    const TEMPLATE_VARS: &[(&str, &[&str])] = &[
        ("admin_account_recovery", &["org_name", "user_name", "fallback_2fa_email", "reset_2fa", "reset_password"]),
        ("change_email", &["token"]),
        ("change_email_existing", &["existing_address", "acting_address"]),
        ("change_email_invited", &["existing_address", "acting_address", "url"]),
        ("delete_account", &["email", "token", "url", "user_id"]),
        ("emergency_access_invite_accepted", &["grantee_email", "url"]),
        ("emergency_access_invite_confirmed", &["grantor_name", "url"]),
        ("emergency_access_recovery_approved", &["grantor_name", "url"]),
        ("emergency_access_recovery_initiated", &["atype", "grantee_name", "url", "wait_time_days"]),
        ("emergency_access_recovery_rejected", &["grantor_name"]),
        ("emergency_access_recovery_reminder", &["atype", "days_left", "grantee_name", "url"]),
        ("emergency_access_recovery_timed_out", &["atype", "grantee_name", "url"]),
        ("incomplete_2fa_login", &["datetime", "device_name", "device_type", "ip", "time_limit"]),
        ("invite_accepted", &["email", "org_name", "url"]),
        ("invite_confirmed", &["org_name", "url"]),
        ("new_device_logged_in", &["datetime", "device_name", "device_type", "ip", "url"]),
        ("protected_action", &["token"]),
        ("pw_hint_none", &["url"]),
        ("pw_hint_some", &["hint", "url"]),
        ("recover_twofactor", &["datetime", "ip"]),
        ("register_verify_email", &["url"]),
        ("send_2fa_removed_from_org", &["org_name"]),
        ("send_emergency_access_invite", &["grantor_name", "url"]),
        ("send_org_invite", &["org_name", "url"]),
        ("send_single_org_removed_from_org", &["org_name"]),
        ("smtp_test", &["url"]),
        ("sso_change_email", &["url"]),
        ("twofactor_email", &["token"]),
        ("verify_email", &["token", "url", "user_id"]),
        ("welcome", &["url"]),
        ("welcome_must_verify", &["token", "url", "user_id"]),
    ];

    fn sample_data(vars: &[&str]) -> serde_json::Value {
        let mut data = serde_json::Map::new();
        for var in vars.iter().copied().chain(["img_src"]) {
            let value = if BOOLEAN_VARS.contains(&var) {
                json!(true)
            } else {
                json!(format!("sample-{var}"))
            };
            data.insert(var.to_owned(), value);
        }
        serde_json::Value::Object(data)
    }

    fn english_template_names() -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(EMAIL_TEMPLATES_DIR)
            .unwrap()
            .filter_map(|entry| {
                entry.unwrap().file_name().into_string().unwrap().strip_suffix(".hbs").map(str::to_owned)
            })
            .filter(|name| name.strip_suffix(".html").is_none() && !SHARED_PARTIALS.contains(&name.as_str()))
            .collect();
        names.sort();
        names
    }

    fn render_pair(template_name: &str, data: &serde_json::Value) -> Result<(String, String), Error> {
        get_template(template_name, data)
    }

    /// A templates folder under the system temp dir that the isolated config loads on top of the embedded templates.
    fn templates_folder_with(test_name: &str, files: &[(&str, &str)]) -> String {
        let folder = std::env::temp_dir().join(format!("vaultwarden_mail_test_{test_name}"));
        drop(std::fs::remove_dir_all(&folder));
        for (relative_path, content) in files {
            let path = folder.join(relative_path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        folder.to_str().unwrap().to_owned()
    }

    #[test]
    fn test_sample_data_table_covers_every_english_template() {
        let table: Vec<&str> = TEMPLATE_VARS.iter().map(|(name, _)| *name).collect();
        assert_eq!(
            english_template_names(),
            table,
            "English templates in {EMAIL_TEMPLATES_DIR} vs the sample data table"
        );
    }

    #[test]
    fn test_sample_data() {
        let data = sample_data(&["url", "reset_2fa"]);
        assert_eq!(
            data,
            json!({"url": "sample-url", "reset_2fa": true, "img_src": "sample-img_src"}),
            "sample_data(url, reset_2fa)"
        );
    }

    #[test]
    fn test_every_russian_email_renders_with_the_data_of_its_english_original() {
        if !in_isolated_config(&[]) {
            return;
        }
        for (name, vars) in TEMPLATE_VARS {
            let data = sample_data(vars);
            for suffix in ["", ".html"] {
                let english_name = format!("email/{name}{suffix}");
                let russian_name = format!("email/ru/{name}{suffix}");
                let (english_subject, english_body) = render_pair(&english_name, &data)
                    .unwrap_or_else(|e| panic!("{english_name} with {data} failed: {e:?}"));
                let (russian_subject, russian_body) = render_pair(&russian_name, &data)
                    .unwrap_or_else(|e| panic!("{russian_name} with {data} failed: {e:?}"));
                assert_ne!(
                    english_subject, russian_subject,
                    "{russian_name} subject is not translated: {russian_subject:?}"
                );
                assert_ne!(english_body, russian_body, "{russian_name} body is not translated");
            }
        }
    }

    #[test]
    fn test_render_text_picks_the_template_of_the_locale() {
        if !in_isolated_config(&[]) {
            return;
        }
        let data = sample_data(&["url"]);
        let english = render_text(Locale::En, WELCOME, &data).unwrap();
        let russian = render_text(Locale::Ru, WELCOME, &data).unwrap();
        let expected_english = (
            render_pair("email/welcome.html", &data).unwrap().0,
            render_pair("email/welcome.html", &data).unwrap().1,
            render_pair("email/welcome", &data).unwrap().1,
        );
        let expected_russian = (
            render_pair("email/ru/welcome.html", &data).unwrap().0,
            render_pair("email/ru/welcome.html", &data).unwrap().1,
            render_pair("email/ru/welcome", &data).unwrap().1,
        );
        assert_eq!(english, expected_english, "render_text(En, {WELCOME})");
        assert_eq!(russian, expected_russian, "render_text(Ru, {WELCOME})");
        assert_ne!(english.0, russian.0, "the En and Ru subjects must differ");
    }

    #[test]
    fn test_render_text_falls_back_to_english_when_russian_template_fails() {
        let broken = format!("{{{{no_such_variable}}}}{TEMPLATE_BODY_SEPARATOR}body");
        let folder = templates_folder_with(
            "failing_ru",
            &[("email/ru/welcome.hbs", &broken), ("email/ru/welcome.html.hbs", &broken)],
        );
        if !in_isolated_config(&[("TEMPLATES_FOLDER", &folder)]) {
            return;
        }
        let data = sample_data(&["url"]);
        let russian_result = render_text(Locale::Ru, WELCOME, &data);
        let english_result = render_text(Locale::En, WELCOME, &data);
        assert_eq!(
            russian_result.as_ref().unwrap(),
            english_result.as_ref().unwrap(),
            "broken ru template: Ru {russian_result:?} vs En {english_result:?}"
        );
    }

    #[test]
    fn test_render_text_falls_back_to_english_when_russian_template_is_missing() {
        let page = format!("Only English{TEMPLATE_BODY_SEPARATOR}body {{{{url}}}}");
        let folder = templates_folder_with(
            "missing_ru",
            &[("email/only_english.hbs", &page), ("email/only_english.html.hbs", &page)],
        );
        if !in_isolated_config(&[("TEMPLATES_FOLDER", &folder)]) {
            return;
        }
        let data = sample_data(&["url"]);
        let (subject, body_html, body_text) = render_text(Locale::Ru, ONLY_ENGLISH, &data)
            .unwrap_or_else(|e| panic!("Ru request for {ONLY_ENGLISH} without ru template failed: {e:?}"));
        assert_eq!(
            (subject.as_str(), body_html.as_str(), body_text.as_str()),
            ("Only English", "body sample-url", "body sample-url"),
            "Ru request for {ONLY_ENGLISH} without ru template"
        );
    }

    #[test]
    fn test_render_text_fails_when_english_template_is_missing_too() {
        if !in_isolated_config(&[]) {
            return;
        }
        let result = render_text(Locale::Ru, "email/no_such_template", &sample_data(&[]));
        assert!(result.is_err(), "a template without English original must be an error, got {result:?}");
    }

    #[test]
    fn test_templates_folder_overrides_embedded_russian_template() {
        let page = format!("{SAMPLE_OVERRIDE_SUBJECT}{TEMPLATE_BODY_SEPARATOR}body");
        let folder = templates_folder_with(
            "override_ru",
            &[("email/ru/welcome.hbs", &page), ("email/ru/welcome.html.hbs", &page)],
        );
        if !in_isolated_config(&[("TEMPLATES_FOLDER", &folder)]) {
            return;
        }
        let (subject, ..) = render_text(Locale::Ru, WELCOME, &sample_data(&["url"])).unwrap();
        assert_eq!(subject, SAMPLE_OVERRIDE_SUBJECT, "user file email/ru/welcome.hbs must win over the embedded one");
    }

    #[test]
    fn test_get_text_uses_server_default_locale_for_unknown_address() {
        if !in_isolated_config(&[("EMAIL_DEFAULT_LOCALE", "ru")]) {
            return;
        }
        let data = json!({"url": "sample-url", "img_src": "sample-img_src"});
        let (subject, ..) = futures::executor::block_on(get_text(WELCOME, "nobody@example.com", data.clone())).unwrap();
        let expected = render_pair("email/ru/welcome.html", &data).unwrap().0;
        assert_eq!(subject, expected, "get_text for an unknown address with EMAIL_DEFAULT_LOCALE=ru");
    }

    #[test]
    fn test_get_text_strips_html_from_data() {
        if !in_isolated_config(&[]) {
            return;
        }
        let data = json!({"url": "<b>bold</b>", "img_src": "sample-img_src"});
        let (_, body_html, body_text) = futures::executor::block_on(get_text(WELCOME, "a@example.com", data)).unwrap();
        assert!(
            !body_html.contains("<b>bold") && !body_text.contains("<b>bold"),
            "tags in data must be stripped, got html {body_html:?} text {body_text:?}"
        );
    }
}
