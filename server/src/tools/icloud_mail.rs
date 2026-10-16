//! iCloud Mail over IMAP (imap.mail.me.com) and SMTP (smtp.mail.me.com) with an app-specific password.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::{ToolError, ToolResult};
use crate::http::{bounded, clip, render};
use crate::server::AppleServer;
use crate::util::strip_html;

const SECTION: &str = "icloud";
const IMAP_HOST: &str = "imap.mail.me.com";
const SMTP_HOST: &str = "smtp.mail.me.com";

type Session = imap::Session<imap::Connection>;

pub(crate) fn imap_connect(user: &str, pass: &str) -> Result<Session, ToolError> {
    let client = imap::ClientBuilder::new(IMAP_HOST, 993).connect().map_err(|e| ToolError::new(format!("IMAP connect failed: {e}")))?;
    client.login(user, pass).map_err(|(e, _)| ToolError::new(format!("IMAP login failed: {e}. Use the iCloud email as icloud.apple_id and an app-specific password.")))
}

fn imap_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn imap_date(s: &str) -> Result<String, ToolError> {
    let d = crate::util::parse_datetime(s, None)?;
    Ok(d.format("%d-%b-%Y").to_string())
}

fn header_summary(raw: &[u8]) -> Value {
    use mailparse::MailHeaderMap;
    match mailparse::parse_headers(raw) {
        Ok((headers, _)) => json!({
            "from": headers.get_first_value("From"),
            "to": headers.get_first_value("To"),
            "cc": headers.get_first_value("Cc"),
            "subject": headers.get_first_value("Subject"),
            "date": headers.get_first_value("Date"),
            "message_id": headers.get_first_value("Message-ID"),
        }),
        Err(e) => json!({"header_error": e.to_string()}),
    }
}

fn collect_parts(part: &mailparse::ParsedMail, texts: &mut Vec<String>, htmls: &mut Vec<String>, attachments: &mut Vec<Value>) {
    let disp = part.get_content_disposition();
    let is_attachment = disp.disposition == mailparse::DispositionType::Attachment || (disp.params.contains_key("filename") && !part.ctype.mimetype.starts_with("text/"));
    if is_attachment {
        attachments.push(json!({"filename": disp.params.get("filename").or_else(|| part.ctype.params.get("name")), "mime": part.ctype.mimetype, "size": part.get_body_raw().map(|b| b.len()).unwrap_or(0)}));
        return;
    }
    if part.subparts.is_empty() {
        match part.ctype.mimetype.as_str() {
            "text/plain" => texts.push(part.get_body().unwrap_or_default()),
            "text/html" => htmls.push(part.get_body().unwrap_or_default()),
            m if m.starts_with("image/") || m.starts_with("application/") => attachments.push(json!({"filename": part.ctype.params.get("name"), "mime": m, "size": part.get_body_raw().map(|b| b.len()).unwrap_or(0)})),
            _ => {}
        }
    } else {
        for sp in &part.subparts {
            collect_parts(sp, texts, htmls, attachments);
        }
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MailListParams {
    /// Mailbox (default INBOX); others: "Sent Messages", Drafts, "Deleted Messages", Junk, Archive, or any folder from icloud_mail_folders
    pub folder: Option<String>,
    /// Max messages, newest first (default 20)
    pub limit: Option<u32>,
    /// Only unread (default false)
    pub unseen_only: Option<bool>,
    /// Only messages since this date (YYYY-MM-DD)
    pub since: Option<String>,
    /// Filter: sender contains
    pub from: Option<String>,
    /// Filter: subject contains
    pub subject: Option<String>,
    /// Filter: body/headers contain
    pub text: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MailReadParams {
    /// Message UID (from icloud_mail_list)
    pub uid: u32,
    /// Mailbox (default INBOX)
    pub folder: Option<String>,
    /// Max body characters (default 15000)
    pub max_chars: Option<u32>,
    /// Mark as read after reading (default false)
    pub mark_seen: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MailSendParams {
    /// Recipients, comma-separated ("Name <a@b.c>" accepted)
    pub to: String,
    pub cc: Option<String>,
    pub bcc: Option<String>,
    pub subject: String,
    /// Message body (plain text, or HTML when html=true)
    pub body: String,
    /// Body is HTML (a plain-text alternative is derived automatically)
    pub html: Option<bool>,
    pub reply_to: Option<String>,
    /// In-Reply-To message id to thread a reply
    pub in_reply_to: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MailFlagParams {
    /// Message UID
    pub uid: u32,
    /// Mailbox (default INBOX)
    pub folder: Option<String>,
    /// Set read (true) / unread (false)
    pub seen: Option<bool>,
    /// Set / clear the flag (star)
    pub flagged: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MailMoveParams {
    /// Message UID
    pub uid: u32,
    /// Source mailbox (default INBOX)
    pub folder: Option<String>,
    /// Destination mailbox, e.g. Archive, "Deleted Messages" (= trash), Junk
    pub destination: String,
}

impl AppleServer {
    pub(crate) fn mail_creds(&self) -> Result<(String, String), ToolError> {
        let cfg = self.cfg();
        Ok((cfg.require(SECTION, "apple_id")?, cfg.require(SECTION, "app_specific_password")?))
    }
}

#[tool_router(router = icloud_mail_router, vis = "pub")]
impl AppleServer {
    /// List iCloud Mail folders (mailboxes) with message counts.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn icloud_mail_folders(&self) -> ToolResult {
        let (user, pass) = self.mail_creds()?;
        let out = tokio::task::spawn_blocking(move || -> Result<Value, ToolError> {
            let mut s = imap_connect(&user, &pass)?;
            let names = s.list(Some(""), Some("*")).map_err(|e| ToolError::new(e.to_string()))?;
            let mut folders = Vec::new();
            for n in names.iter() {
                let name = n.name().to_string();
                let counts = s.status(&name, "(MESSAGES UNSEEN)").ok().map(|m| json!({"messages": m.exists, "unseen": m.unseen})).unwrap_or(Value::Null);
                folders.push(json!({"name": name, "attributes": n.attributes().iter().map(|a| format!("{a:?}")).collect::<Vec<_>>(), "status": counts}));
            }
            let _ = s.logout();
            Ok(json!({"folders": folders}))
        })
        .await
        .map_err(|e| ToolError::new(e.to_string()))??;
        Ok(render(&out))
    }

    /// List recent iCloud Mail messages in a folder (uid, from, subject, date, flags), with optional filters.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn icloud_mail_list(&self, Parameters(p): Parameters<MailListParams>) -> ToolResult {
        let (user, pass) = self.mail_creds()?;
        let folder = p.folder.clone().unwrap_or_else(|| "INBOX".into());
        let limit = p.limit.unwrap_or(20).clamp(1, 200) as usize;
        let mut query = vec![if p.unseen_only.unwrap_or(false) { "UNSEEN".to_string() } else { "ALL".to_string() }];
        if let Some(s) = &p.since { query.push(format!("SINCE {}", imap_date(s)?)); }
        if let Some(f) = &p.from { query.push(format!("FROM {}", imap_quote(f))); }
        if let Some(s) = &p.subject { query.push(format!("SUBJECT {}", imap_quote(s))); }
        if let Some(t) = &p.text { query.push(format!("TEXT {}", imap_quote(t))); }
        let query = query.join(" ");
        let out = tokio::task::spawn_blocking(move || -> Result<Value, ToolError> {
            let mut s = imap_connect(&user, &pass)?;
            let mbox = s.examine(&folder).map_err(|e| ToolError::new(format!("cannot open folder {folder}: {e}")))?;
            let uids = s.uid_search(&query).map_err(|e| ToolError::new(format!("IMAP search failed ({query}): {e}")))?;
            let mut uids: Vec<u32> = uids.into_iter().collect();
            uids.sort_unstable_by(|a, b| b.cmp(a));
            let total = uids.len();
            uids.truncate(limit);
            let mut messages = Vec::new();
            if !uids.is_empty() {
                let set = uids.iter().map(|u| u.to_string()).collect::<Vec<_>>().join(",");
                let fetches = s.uid_fetch(set, "(UID FLAGS INTERNALDATE RFC822.SIZE BODY.PEEK[HEADER.FIELDS (FROM TO CC SUBJECT DATE MESSAGE-ID)])").map_err(|e| ToolError::new(e.to_string()))?;
                for f in fetches.iter() {
                    let mut m = f.header().map(header_summary).unwrap_or(json!({}));
                    m["uid"] = json!(f.uid);
                    m["flags"] = json!(f.flags().iter().map(|fl| format!("{fl:?}")).collect::<Vec<_>>());
                    m["size"] = json!(f.size);
                    m["internal_date"] = json!(f.internal_date().map(|d| d.to_rfc3339()));
                    messages.push(m);
                }
                messages.sort_by(|a, b| b["uid"].as_u64().cmp(&a["uid"].as_u64()));
            }
            let _ = s.logout();
            Ok(json!({"folder": folder, "folder_messages": mbox.exists, "matches": total, "messages": messages}))
        })
        .await
        .map_err(|e| ToolError::new(e.to_string()))??;
        Ok(render(&out))
    }

    /// Read one iCloud Mail message: headers, text body (HTML converted to text when needed), attachment list.
    #[tool(annotations(read_only_hint = true, open_world_hint = true))]
    pub async fn icloud_mail_read(&self, Parameters(p): Parameters<MailReadParams>) -> ToolResult {
        let (user, pass) = self.mail_creds()?;
        let folder = p.folder.clone().unwrap_or_else(|| "INBOX".into());
        let max_chars = p.max_chars.unwrap_or(15_000) as usize;
        let mark_seen = p.mark_seen.unwrap_or(false);
        let uid = p.uid;
        let out = tokio::task::spawn_blocking(move || -> Result<Value, ToolError> {
            let mut s = imap_connect(&user, &pass)?;
            if mark_seen { s.select(&folder) } else { s.examine(&folder) }.map_err(|e| ToolError::new(format!("cannot open folder {folder}: {e}")))?;
            let fetches = s.uid_fetch(uid.to_string(), if mark_seen { "(UID FLAGS BODY[])" } else { "(UID FLAGS BODY.PEEK[])" }).map_err(|e| ToolError::new(e.to_string()))?;
            let f = fetches.iter().next().ok_or_else(|| ToolError::new(format!("no message with UID {uid} in {folder}")))?;
            let raw = f.body().ok_or_else(|| ToolError::new("empty message body"))?;
            let mail = mailparse::parse_mail(raw).map_err(|e| ToolError::new(format!("MIME parse error: {e}")))?;
            let mut texts = Vec::new();
            let mut htmls = Vec::new();
            let mut attachments = Vec::new();
            collect_parts(&mail, &mut texts, &mut htmls, &mut attachments);
            let body = if !texts.is_empty() { texts.join("\n---\n") } else { htmls.iter().map(|h| strip_html(h)).collect::<Vec<_>>().join("\n---\n") };
            let mut out = header_summary(raw);
            out["uid"] = json!(f.uid);
            out["flags"] = json!(f.flags().iter().map(|fl| format!("{fl:?}")).collect::<Vec<_>>());
            out["body"] = json!(clip(&body, max_chars));
            out["body_format"] = json!(if !texts.is_empty() { "text" } else { "html→text" });
            out["attachments"] = Value::Array(attachments);
            let _ = s.logout();
            Ok(out)
        })
        .await
        .map_err(|e| ToolError::new(e.to_string()))??;
        Ok(render(&out))
    }

    /// Send an email from the iCloud account via SMTP (plain text or HTML).
    #[tool(annotations(open_world_hint = true))]
    pub async fn icloud_mail_send(&self, Parameters(p): Parameters<MailSendParams>) -> ToolResult {
        use lettre::message::{Mailbox, MultiPart, header::ContentType};
        use lettre::transport::smtp::authentication::Credentials;
        use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
        let (user, pass) = self.mail_creds()?;
        let cfg = self.cfg();
        let from_addr = cfg.get_str(SECTION, "mail_from").unwrap_or_else(|| user.clone());
        let from: Mailbox = match cfg.get_str(SECTION, "mail_from_name") {
            Some(name) => Mailbox::new(Some(name), from_addr.parse().map_err(|e| ToolError::new(format!("bad mail_from: {e}")))?),
            None => from_addr.parse().map_err(|e| ToolError::new(format!("bad mail_from: {e}")))?,
        };
        let parse_list = |s: &str| -> Result<Vec<Mailbox>, ToolError> {
            s.split(',').map(|a| a.trim()).filter(|a| !a.is_empty()).map(|a| a.parse::<Mailbox>().map_err(|e| ToolError::new(format!("bad address `{a}`: {e}")))).collect()
        };
        let mut builder = Message::builder().from(from).subject(p.subject.clone());
        for m in parse_list(&p.to)? { builder = builder.to(m); }
        if let Some(cc) = &p.cc { for m in parse_list(cc)? { builder = builder.cc(m); } }
        if let Some(bcc) = &p.bcc { for m in parse_list(bcc)? { builder = builder.bcc(m); } }
        if let Some(r) = &p.reply_to { for m in parse_list(r)? { builder = builder.reply_to(m); } }
        if let Some(irt) = &p.in_reply_to { builder = builder.in_reply_to(irt.clone()).references(irt.clone()); }
        let message = if p.html.unwrap_or(false) {
            builder.multipart(MultiPart::alternative_plain_html(strip_html(&p.body), p.body.clone()))
        } else {
            builder.header(ContentType::TEXT_PLAIN).body(p.body.clone())
        }
        .map_err(|e| ToolError::new(format!("cannot build message: {e}")))?;
        let mailer = AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(SMTP_HOST)
            .map_err(|e| ToolError::new(e.to_string()))?
            .port(587)
            .credentials(Credentials::new(user, pass))
            .build();
        let resp = mailer.send(message).await.map_err(|e| ToolError::new(format!("SMTP send failed: {e}. If the sender is not your @icloud.com/@me.com address, set icloud.mail_from to an iCloud address or alias.")))?;
        Ok(bounded(format!("Sent to {} (SMTP: {}).", p.to, resp.message().collect::<Vec<_>>().join(" "))))
    }

    /// Mark an iCloud Mail message read/unread or flagged/unflagged.
    #[tool(annotations(open_world_hint = true))]
    pub async fn icloud_mail_flag(&self, Parameters(p): Parameters<MailFlagParams>) -> ToolResult {
        let (user, pass) = self.mail_creds()?;
        let folder = p.folder.clone().unwrap_or_else(|| "INBOX".into());
        let uid = p.uid;
        let (seen, flagged) = (p.seen, p.flagged);
        if seen.is_none() && flagged.is_none() {
            crate::bail!("pass seen and/or flagged");
        }
        let out = tokio::task::spawn_blocking(move || -> Result<String, ToolError> {
            let mut s = imap_connect(&user, &pass)?;
            s.select(&folder).map_err(|e| ToolError::new(format!("cannot open folder {folder}: {e}")))?;
            let mut applied = Vec::new();
            for (flag, val) in [("\\Seen", seen), ("\\Flagged", flagged)] {
                if let Some(v) = val {
                    let cmd = format!("{}FLAGS.SILENT ({flag})", if v { "+" } else { "-" });
                    s.uid_store(uid.to_string(), &cmd).map_err(|e| ToolError::new(e.to_string()))?;
                    applied.push(cmd);
                }
            }
            let _ = s.logout();
            Ok(format!("UID {uid} in {folder}: {}", applied.join(", ")))
        })
        .await
        .map_err(|e| ToolError::new(e.to_string()))??;
        Ok(out)
    }

    /// Move an iCloud Mail message to another folder (use "Deleted Messages" to trash it, Archive to archive).
    #[tool(annotations(destructive_hint = true, open_world_hint = true))]
    pub async fn icloud_mail_move(&self, Parameters(p): Parameters<MailMoveParams>) -> ToolResult {
        let (user, pass) = self.mail_creds()?;
        let folder = p.folder.clone().unwrap_or_else(|| "INBOX".into());
        let dest = p.destination.clone();
        let uid = p.uid;
        let out = tokio::task::spawn_blocking(move || -> Result<String, ToolError> {
            let mut s = imap_connect(&user, &pass)?;
            s.select(&folder).map_err(|e| ToolError::new(format!("cannot open folder {folder}: {e}")))?;
            s.uid_mv(uid.to_string(), &dest).map_err(|e| ToolError::new(format!("move failed: {e}")))?;
            let _ = s.logout();
            Ok(format!("UID {uid} moved from {folder} to {dest}."))
        })
        .await
        .map_err(|e| ToolError::new(e.to_string()))??;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_and_dates() {
        assert_eq!(imap_quote(r#"a "b" \c"#), r#""a \"b\" \\c""#);
        assert_eq!(imap_date("2026-10-16").unwrap(), "16-Oct-2026");
    }

    #[test]
    fn parses_mime() {
        let raw = b"From: A <a@x.y>\r\nTo: b@x.y\r\nSubject: =?UTF-8?Q?Caf=C3=A9?=\r\nContent-Type: multipart/alternative; boundary=\"B\"\r\n\r\n--B\r\nContent-Type: text/plain\r\n\r\nHello\r\n--B\r\nContent-Type: text/html\r\n\r\n<b>Hello</b>\r\n--B--\r\n";
        let h = header_summary(raw);
        assert_eq!(h["subject"], "Café");
        let mail = mailparse::parse_mail(raw).unwrap();
        let (mut t, mut hh, mut a) = (vec![], vec![], vec![]);
        collect_parts(&mail, &mut t, &mut hh, &mut a);
        assert_eq!(t, vec!["Hello"]);
        assert_eq!(hh.len(), 1);
        assert!(a.is_empty());
    }
}
