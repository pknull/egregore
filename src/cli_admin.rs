use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use anyhow::{anyhow, bail, Context};
use base64::Engine;
use serde::Serialize;

use egregore::api::routes_peers::is_valid_peer_address;
use egregore::config::Config;
use egregore::feed::schema::{SchemaDefinition, SchemaFileDefinition, SchemaRegistry};
use egregore::feed::store::retention::{RetentionPolicy, RetentionScope};
use egregore::feed::store::{AddressPeer, ConsumerGroup, FeedStore, GroupMember, PeerRecord};
use egregore::identity::{Identity, PublicId};

use crate::{Command, GroupCommand, PeerCommand, RetentionCommand, SchemaCommand, TopicCommand};

#[derive(Clone)]
pub struct CliContext {
    pub config: Config,
    pub store: FeedStore,
    pub identity: Identity,
}

#[derive(Clone, Copy)]
pub struct OutputMode {
    pub json: bool,
    pub quiet: bool,
}

impl OutputMode {
    fn emit<T, F>(&self, value: &T, human: F) -> anyhow::Result<()>
    where
        T: Serialize,
        F: FnOnce() -> String,
    {
        if self.quiet {
            return Ok(());
        }

        if self.json {
            println!("{}", serde_json::to_string_pretty(value)?);
        } else {
            let text = human();
            if !text.is_empty() {
                println!("{text}");
            }
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Serialize)]
struct FollowChange {
    author: String,
}

#[derive(Debug, Clone, Serialize)]
struct GroupSummary {
    group_id: String,
    generation: u64,
    member_count: u64,
    created_at: String,
    updated_at: String,
}

#[derive(Debug, Clone, Serialize)]
struct GroupMemberView {
    member_id: String,
    joined_at: String,
    last_heartbeat: String,
    assignment_generation: u64,
    assigned_feeds: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct GroupDetail {
    group: GroupSummary,
    members: Vec<GroupMemberView>,
}

#[derive(Debug, Clone, Serialize)]
struct SchemaView {
    schema_id: String,
    content_type: String,
    version: u32,
    codec: String,
    compatibility: String,
    description: Option<String>,
    json_schema: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
struct RetentionView {
    id: i64,
    scope: String,
    max_age_secs: Option<u64>,
    max_messages: Option<u64>,
    max_bytes: Option<u64>,
    compact_key: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct PeerListEntry {
    address: String,
    public_id: Option<String>,
    source: String,
}

#[derive(Debug, Clone, Serialize)]
struct PeerStatusEntry {
    address: Option<String>,
    public_id: Option<String>,
    sources: Vec<String>,
    first_seen: Option<String>,
    last_connected: Option<String>,
    last_synced: Option<String>,
    private: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
struct IdentityView {
    public_id: String,
    x25519_public: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    secret_key_base64: Option<String>,
}

pub fn handle_command(
    command: &Command,
    ctx: &CliContext,
    output: OutputMode,
) -> anyhow::Result<bool> {
    match command {
        Command::Follow { author } => {
            let author = parse_public_id(author)?;
            ctx.store.add_follow(&author)?;
            let value = FollowChange {
                author: author.0.clone(),
            };
            output.emit(&value, || format!("Following {}", value.author))?;
            Ok(true)
        }
        Command::Follows => {
            let mut follows: Vec<String> = ctx
                .store
                .get_follows()?
                .into_iter()
                .map(|id| id.0)
                .collect();
            follows.sort();
            output.emit(&follows, || {
                format_string_list(&follows, "No follows configured.")
            })?;
            Ok(true)
        }
        Command::Unfollow { author } => {
            let author = parse_public_id(author)?;
            ctx.store.remove_follow(&author)?;
            let value = FollowChange {
                author: author.0.clone(),
            };
            output.emit(&value, || format!("Unfollowed {}", value.author))?;
            Ok(true)
        }
        Command::Topic { command } => {
            handle_topic_command(command, ctx, output)?;
            Ok(true)
        }
        Command::Group { command } => {
            handle_group_command(command, ctx, output)?;
            Ok(true)
        }
        Command::Schema { command } => {
            handle_schema_command(command, ctx, output)?;
            Ok(true)
        }
        Command::Retention { command } => {
            handle_retention_command(command, ctx, output)?;
            Ok(true)
        }
        Command::Peer { command } => {
            handle_peer_command(command, ctx, output)?;
            Ok(true)
        }
        Command::Identity { export } => {
            let view = identity_view(&ctx.identity, *export);
            output.emit(&view, || format_identity_view(&view, *export))?;
            Ok(true)
        }
        Command::Update { .. } => Ok(false),
    }
}

fn handle_topic_command(
    command: &TopicCommand,
    ctx: &CliContext,
    output: OutputMode,
) -> anyhow::Result<()> {
    match command {
        TopicCommand::Subscribe { name } => {
            ctx.store.add_topic_subscription(name)?;
            output.emit(&serde_json::json!({ "topic": name }), || {
                format!("Subscribed to topic {}", name)
            })
        }
        TopicCommand::List => {
            let mut topics = ctx.store.get_topic_subscriptions()?;
            topics.sort();
            output.emit(&topics, || {
                format_string_list(&topics, "No topic subscriptions.")
            })?;
            Ok(())
        }
        TopicCommand::Unsubscribe { name } => {
            ctx.store.remove_topic_subscription(name)?;
            output.emit(&serde_json::json!({ "topic": name }), || {
                format!("Unsubscribed from topic {}", name)
            })
        }
    }
}

fn handle_group_command(
    command: &GroupCommand,
    ctx: &CliContext,
    output: OutputMode,
) -> anyhow::Result<()> {
    match command {
        GroupCommand::Create { name, members } => {
            validate_group_name(name)?;
            let members = parse_members_csv(members.as_deref())?;
            ctx.store.create_group(name)?;
            for member in members {
                ctx.store.join_group(name, &member)?;
            }
            let detail = group_detail(&ctx.store, name)?
                .ok_or_else(|| anyhow!("group disappeared after creation: {name}"))?;
            output.emit(&detail, || {
                format!(
                    "Created group {} (generation {}, members {})",
                    detail.group.group_id, detail.group.generation, detail.group.member_count
                )
            })
        }
        GroupCommand::List => {
            let groups = list_groups(&ctx.store)?;
            output.emit(&groups, || format_group_list(&groups))?;
            Ok(())
        }
        GroupCommand::Show { name } => {
            let detail = group_detail(&ctx.store, name)?
                .ok_or_else(|| anyhow!("group not found: {name}"))?;
            output.emit(&detail, || format_group_detail(&detail))?;
            Ok(())
        }
        GroupCommand::Delete { name } => {
            if !ctx.store.delete_group(name)? {
                bail!("group not found: {}", name);
            }
            output.emit(&serde_json::json!({ "group_id": name }), || {
                format!("Deleted group {}", name)
            })
        }
    }
}

fn handle_schema_command(
    command: &SchemaCommand,
    ctx: &CliContext,
    output: OutputMode,
) -> anyhow::Result<()> {
    let schemas_dir = ctx.config.schemas_dir();
    match command {
        SchemaCommand::Register { content_type, file } => {
            let schema =
                register_schema_file(content_type, file, &schemas_dir, ctx.config.schema_strict)?;
            let view = schema_view(&schema);
            output.emit(&view, || format!("Registered schema {}", view.schema_id))
        }
        SchemaCommand::List => {
            let mut schemas: Vec<_> = schema_registry(ctx.config.schema_strict, &schemas_dir)
                .list_all()
                .into_iter()
                .map(|schema| schema_view(&schema))
                .collect();
            schemas.sort_by(|left, right| left.schema_id.cmp(&right.schema_id));
            output.emit(&schemas, || format_schema_list(&schemas))?;
            Ok(())
        }
        SchemaCommand::Show { schema } => {
            let registry = schema_registry(ctx.config.schema_strict, &schemas_dir);
            let resolved = if schema.contains("/v") {
                registry.get(schema)
            } else {
                registry.get_latest(schema)
            }
            .ok_or_else(|| anyhow!("schema not found: {}", schema))?;
            let view = schema_view(&resolved);
            output.emit(&view, || format_schema_detail(&view))?;
            Ok(())
        }
    }
}

fn handle_retention_command(
    command: &RetentionCommand,
    ctx: &CliContext,
    output: OutputMode,
) -> anyhow::Result<()> {
    match command {
        RetentionCommand::Set {
            max_age,
            max_messages,
        } => {
            let max_age_secs = match max_age {
                Some(value) => Some(parse_duration_spec(value)?),
                None => None,
            };
            if max_age_secs.is_none() && max_messages.is_none() {
                bail!("retention set requires --max-age and/or --max-messages");
            }

            let policy = RetentionPolicy {
                id: None,
                scope: RetentionScope::Global,
                max_age_secs,
                max_count: *max_messages,
                max_bytes: None,
                compact_key: None,
            };
            ctx.store.save_retention_policy(&policy)?;
            let view = show_global_retention(&ctx.store)?
                .ok_or_else(|| anyhow!("failed to load global retention policy after update"))?;
            output.emit(&view, || format_retention_view(&view))?;
            Ok(())
        }
        RetentionCommand::Show => {
            let view = show_global_retention(&ctx.store)?;
            output.emit(&view, || match &view {
                Some(policy) => format_retention_view(policy),
                None => "No global retention policy configured.".to_string(),
            })?;
            Ok(())
        }
    }
}

fn handle_peer_command(
    command: &PeerCommand,
    ctx: &CliContext,
    output: OutputMode,
) -> anyhow::Result<()> {
    match command {
        PeerCommand::Add { address } => {
            if !is_valid_peer_address(address) {
                bail!("address must be in host:port format");
            }
            ctx.store.insert_address_peer(address)?;
            output.emit(&serde_json::json!({ "address": address }), || {
                format!("Added peer {}", address)
            })
        }
        PeerCommand::List => {
            let peers = collect_peer_list(ctx)?;
            output.emit(&peers, || format_peer_list(&peers))?;
            Ok(())
        }
        PeerCommand::Status => {
            let peers = collect_peer_status(ctx)?;
            output.emit(&peers, || format_peer_status(&peers))?;
            Ok(())
        }
    }
}

fn parse_public_id(value: &str) -> anyhow::Result<PublicId> {
    if !PublicId::is_valid_format(value) {
        bail!("invalid public ID: {}", value);
    }
    Ok(PublicId(value.to_string()))
}

fn parse_members_csv(input: Option<&str>) -> anyhow::Result<Vec<PublicId>> {
    let Some(input) = input else {
        return Ok(Vec::new());
    };

    let mut seen = BTreeSet::new();
    let mut members = Vec::new();
    for part in input.split(',') {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            continue;
        }
        let member = parse_public_id(trimmed)?;
        if seen.insert(member.0.clone()) {
            members.push(member);
        }
    }
    Ok(members)
}

fn validate_group_name(name: &str) -> anyhow::Result<()> {
    let valid = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !valid {
        bail!("group name must be 1-64 alphanumeric characters, hyphens, or underscores");
    }
    Ok(())
}

fn list_groups(store: &FeedStore) -> anyhow::Result<Vec<GroupSummary>> {
    let mut groups = store
        .list_groups()?
        .into_iter()
        .map(|group| group_summary(store, group))
        .collect::<anyhow::Result<Vec<_>>>()?;
    groups.sort_by(|left, right| left.group_id.cmp(&right.group_id));
    Ok(groups)
}

fn group_summary(store: &FeedStore, group: ConsumerGroup) -> anyhow::Result<GroupSummary> {
    Ok(GroupSummary {
        member_count: store.group_member_count(&group.group_id)?,
        group_id: group.group_id,
        generation: group.generation,
        created_at: group.created_at.to_rfc3339(),
        updated_at: group.updated_at.to_rfc3339(),
    })
}

fn group_detail(store: &FeedStore, group_id: &str) -> anyhow::Result<Option<GroupDetail>> {
    let Some(group) = store.get_group(group_id)? else {
        return Ok(None);
    };

    let mut members: Vec<GroupMemberView> = store
        .get_group_members(group_id)?
        .into_iter()
        .map(group_member_view)
        .collect();
    members.sort_by(|left, right| left.member_id.cmp(&right.member_id));

    Ok(Some(GroupDetail {
        group: group_summary(store, group)?,
        members,
    }))
}

fn group_member_view(member: GroupMember) -> GroupMemberView {
    GroupMemberView {
        member_id: member.member_id.0,
        joined_at: member.joined_at.to_rfc3339(),
        last_heartbeat: member.last_heartbeat.to_rfc3339(),
        assignment_generation: member.assignment_generation,
        assigned_feeds: member
            .assigned_feeds
            .into_iter()
            .map(|feed| feed.0)
            .collect(),
    }
}

fn schema_registry(strict: bool, schemas_dir: &Path) -> SchemaRegistry {
    SchemaRegistry::with_schemas_dir(strict, schemas_dir)
}

fn register_schema_file(
    content_type: &str,
    file: &Path,
    schemas_dir: &Path,
    strict: bool,
) -> anyhow::Result<SchemaDefinition> {
    let content = fs::read_to_string(file)
        .with_context(|| format!("failed to read schema file: {}", file.display()))?;
    let raw_json: serde_json::Value = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse JSON in {}", file.display()))?;

    let registry = schema_registry(strict, schemas_dir);
    let latest_version = registry
        .get_latest(content_type)
        .map(|schema| schema.version)
        .unwrap_or(0);

    let schema = if raw_json.get("json_schema").is_some() && raw_json.get("version").is_some() {
        let file_def: SchemaFileDefinition = serde_json::from_value(raw_json.clone())
            .with_context(|| format!("failed to parse schema definition in {}", file.display()))?;
        if file_def.content_type != content_type {
            bail!(
                "schema file content_type '{}' does not match CLI content type '{}'",
                file_def.content_type,
                content_type
            );
        }
        schema_from_file_def(file_def)
    } else {
        SchemaDefinition::new(content_type, latest_version + 1, raw_json)
    };

    registry.register(schema.clone())?;

    fs::create_dir_all(schemas_dir).with_context(|| {
        format!(
            "failed to create schemas directory: {}",
            schemas_dir.display()
        )
    })?;
    let persisted = SchemaFileDefinition {
        content_type: schema.content_type.clone(),
        version: schema.version,
        json_schema: schema.json_schema.clone(),
        codec: Some(schema.codec),
        compatibility: Some(schema.compatibility),
        description: schema.description.clone(),
    };
    let file_name = schema_file_name(&schema.content_type, schema.version);
    let destination = schemas_dir.join(file_name);
    fs::write(&destination, serde_json::to_string_pretty(&persisted)?)
        .with_context(|| format!("failed to write schema file: {}", destination.display()))?;

    let loaded = schema_registry(strict, schemas_dir);
    loaded.get(&schema.schema_id).ok_or_else(|| {
        anyhow!(
            "schema was persisted but could not be reloaded: {}",
            schema.schema_id
        )
    })
}

fn schema_from_file_def(file_def: SchemaFileDefinition) -> SchemaDefinition {
    let mut schema = SchemaDefinition::new(
        file_def.content_type,
        file_def.version,
        file_def.json_schema,
    );
    if let Some(codec) = file_def.codec {
        schema = schema.with_codec(codec);
    }
    if let Some(compatibility) = file_def.compatibility {
        schema = schema.with_compatibility(compatibility);
    }
    if let Some(description) = file_def.description {
        schema = schema.with_description(description);
    }
    schema
}

fn schema_file_name(content_type: &str, version: u32) -> String {
    let safe_type: String = content_type
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    format!("{safe_type}.v{version}.json")
}

fn schema_view(schema: &SchemaDefinition) -> SchemaView {
    SchemaView {
        schema_id: schema.schema_id.clone(),
        content_type: schema.content_type.clone(),
        version: schema.version,
        codec: schema.codec.to_string(),
        compatibility: format!("{:?}", schema.compatibility).to_lowercase(),
        description: schema.description.clone(),
        json_schema: schema.json_schema.clone(),
    }
}

fn show_global_retention(store: &FeedStore) -> anyhow::Result<Option<RetentionView>> {
    let policy = store
        .list_retention_policies()?
        .into_iter()
        .find(|policy| matches!(policy.scope, RetentionScope::Global));
    Ok(policy.map(retention_view))
}

fn retention_view(policy: RetentionPolicy) -> RetentionView {
    RetentionView {
        id: policy.id.unwrap_or_default(),
        scope: "global".to_string(),
        max_age_secs: policy.max_age_secs,
        max_messages: policy.max_count,
        max_bytes: policy.max_bytes,
        compact_key: policy.compact_key,
    }
}

fn parse_duration_spec(input: &str) -> anyhow::Result<u64> {
    if input.trim().is_empty() {
        bail!("duration cannot be empty");
    }

    let mut total = 0u64;
    let mut digits = String::new();
    for ch in input.chars() {
        if ch.is_ascii_digit() {
            digits.push(ch);
            continue;
        }

        if digits.is_empty() {
            bail!("invalid duration segment in '{}'", input);
        }

        let value: u64 = digits.parse()?;
        let multiplier = match ch {
            's' => 1,
            'm' => 60,
            'h' => 60 * 60,
            'd' => 60 * 60 * 24,
            'w' => 60 * 60 * 24 * 7,
            _ => bail!("unsupported duration unit '{}' in '{}'", ch, input),
        };
        total = total
            .checked_add(value.saturating_mul(multiplier))
            .ok_or_else(|| anyhow!("duration is too large: {}", input))?;
        digits.clear();
    }

    if !digits.is_empty() {
        bail!("duration must include a unit suffix: {}", input);
    }

    Ok(total)
}

fn collect_peer_list(ctx: &CliContext) -> anyhow::Result<Vec<PeerListEntry>> {
    let mut peers: Vec<PeerListEntry> = collect_peer_status(ctx)?
        .into_iter()
        .filter_map(|status| {
            status.address.map(|address| PeerListEntry {
                address,
                public_id: status.public_id,
                source: status.sources.join(","),
            })
        })
        .collect();
    peers.sort_by(|left, right| left.address.cmp(&right.address));
    Ok(peers)
}

fn collect_peer_status(ctx: &CliContext) -> anyhow::Result<Vec<PeerStatusEntry>> {
    let mut entries: BTreeMap<String, PeerStatusEntry> = BTreeMap::new();

    for peer in &ctx.config.peers {
        let key = format!("addr:{peer}");
        let entry = entries.entry(key).or_insert_with(|| PeerStatusEntry {
            address: Some(peer.clone()),
            public_id: None,
            sources: Vec::new(),
            first_seen: None,
            last_connected: None,
            last_synced: None,
            private: None,
        });
        push_source(entry, "cli");
    }

    for peer in ctx.store.list_address_peers()? {
        merge_address_peer(&mut entries, peer);
    }

    for peer in ctx.store.list_peers(true)? {
        merge_known_peer(&mut entries, peer);
    }

    Ok(entries.into_values().collect())
}

fn merge_address_peer(entries: &mut BTreeMap<String, PeerStatusEntry>, peer: AddressPeer) {
    let key = format!("addr:{}", peer.address);
    let entry = entries.entry(key).or_insert_with(|| PeerStatusEntry {
        address: Some(peer.address.clone()),
        public_id: peer.public_id.clone(),
        sources: Vec::new(),
        first_seen: None,
        last_connected: None,
        last_synced: None,
        private: None,
    });
    entry.address = Some(peer.address);
    if entry.public_id.is_none() {
        entry.public_id = peer.public_id;
    }
    entry.last_connected = format_ts(peer.last_connected);
    entry.last_synced = format_ts(peer.last_synced);
    push_source(entry, "manual");
}

fn merge_known_peer(entries: &mut BTreeMap<String, PeerStatusEntry>, peer: PeerRecord) {
    let key = if let Some(address) = peer.address.clone() {
        format!("addr:{address}")
    } else {
        format!("pub:{}", peer.public_id.0)
    };
    let entry = entries.entry(key).or_insert_with(|| PeerStatusEntry {
        address: peer.address.clone(),
        public_id: Some(peer.public_id.0.clone()),
        sources: Vec::new(),
        first_seen: Some(peer.first_seen.to_rfc3339()),
        last_connected: format_ts(peer.last_connected),
        last_synced: format_ts(peer.last_synced),
        private: Some(peer.private),
    });
    entry.address = entry.address.clone().or(peer.address);
    entry.public_id = Some(peer.public_id.0);
    entry.first_seen = Some(peer.first_seen.to_rfc3339());
    entry.last_connected = format_ts(peer.last_connected);
    entry.last_synced = format_ts(peer.last_synced);
    entry.private = Some(peer.private);
    push_source(entry, "known");
}

fn push_source(entry: &mut PeerStatusEntry, source: &str) {
    if !entry.sources.iter().any(|existing| existing == source) {
        entry.sources.push(source.to_string());
    }
}

fn format_ts(ts: Option<chrono::DateTime<chrono::Utc>>) -> Option<String> {
    ts.map(|value| value.to_rfc3339())
}

fn identity_view(identity: &Identity, export_secret: bool) -> IdentityView {
    let x25519_public = base64::engine::general_purpose::STANDARD
        .encode(identity.to_x25519_public_key().as_bytes());
    let secret_key_base64 = export_secret
        .then(|| base64::engine::general_purpose::STANDARD.encode(identity.secret_bytes()));
    IdentityView {
        public_id: identity.public_id().0,
        x25519_public,
        secret_key_base64,
    }
}

fn format_string_list(values: &[String], empty_message: &str) -> String {
    if values.is_empty() {
        empty_message.to_string()
    } else {
        values.join("\n")
    }
}

fn format_group_list(groups: &[GroupSummary]) -> String {
    if groups.is_empty() {
        return "No consumer groups.".to_string();
    }
    groups
        .iter()
        .map(|group| {
            format!(
                "{} generation={} members={}",
                group.group_id, group.generation, group.member_count
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_group_detail(detail: &GroupDetail) -> String {
    let mut lines = vec![
        format!("Group: {}", detail.group.group_id),
        format!("Generation: {}", detail.group.generation),
        format!("Members: {}", detail.group.member_count),
        format!("Created: {}", detail.group.created_at),
        format!("Updated: {}", detail.group.updated_at),
    ];

    if detail.members.is_empty() {
        lines.push("Member list: none".to_string());
    } else {
        lines.push("Members:".to_string());
        for member in &detail.members {
            lines.push(format!(
                "{} heartbeat={} feeds={}",
                member.member_id,
                member.last_heartbeat,
                member.assigned_feeds.len()
            ));
        }
    }

    lines.join("\n")
}

fn format_schema_list(schemas: &[SchemaView]) -> String {
    if schemas.is_empty() {
        return "No schemas registered.".to_string();
    }
    schemas
        .iter()
        .map(|schema| {
            let description = schema.description.as_deref().unwrap_or("-");
            format!(
                "{} codec={} {}",
                schema.schema_id, schema.codec, description
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_schema_detail(schema: &SchemaView) -> String {
    let mut lines = vec![
        format!("Schema: {}", schema.schema_id),
        format!("Content type: {}", schema.content_type),
        format!("Version: {}", schema.version),
        format!("Codec: {}", schema.codec),
        format!("Compatibility: {}", schema.compatibility),
    ];
    if let Some(description) = &schema.description {
        lines.push(format!("Description: {}", description));
    }
    lines.push("JSON Schema:".to_string());
    lines.push(serde_json::to_string_pretty(&schema.json_schema).unwrap_or_default());
    lines.join("\n")
}

fn format_retention_view(view: &RetentionView) -> String {
    let max_age = view
        .max_age_secs
        .map(|value| format!("{value}s"))
        .unwrap_or_else(|| "unset".to_string());
    let max_messages = view
        .max_messages
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unset".to_string());
    format!(
        "Global retention: max_age={} max_messages={}",
        max_age, max_messages
    )
}

fn format_peer_list(peers: &[PeerListEntry]) -> String {
    if peers.is_empty() {
        return "No peers configured.".to_string();
    }
    peers
        .iter()
        .map(|peer| match &peer.public_id {
            Some(public_id) => format!("{} [{}] {}", peer.address, peer.source, public_id),
            None => format!("{} [{}]", peer.address, peer.source),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_peer_status(peers: &[PeerStatusEntry]) -> String {
    if peers.is_empty() {
        return "No peer status available.".to_string();
    }
    peers
        .iter()
        .map(|peer| {
            let address = peer.address.as_deref().unwrap_or("-");
            let public_id = peer.public_id.as_deref().unwrap_or("-");
            let connected = peer.last_connected.as_deref().unwrap_or("-");
            let synced = peer.last_synced.as_deref().unwrap_or("-");
            format!(
                "{} public_id={} sources={} last_connected={} last_synced={}",
                address,
                public_id,
                peer.sources.join(","),
                connected,
                synced
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_identity_view(view: &IdentityView, export_secret: bool) -> String {
    let mut lines = vec![
        format!("Public ID: {}", view.public_id),
        format!("X25519 Public: {}", view.x25519_public),
    ];
    if export_secret {
        if let Some(secret) = &view.secret_key_base64 {
            lines.push(format!("Secret Key (base64): {}", secret));
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_duration_supports_days_and_compound_values() {
        assert_eq!(parse_duration_spec("30d").unwrap(), 30 * 24 * 60 * 60);
        assert_eq!(parse_duration_spec("1h30m").unwrap(), 5400);
    }

    #[test]
    fn parse_members_deduplicates_and_preserves_order() {
        let first = Identity::generate().public_id().0;
        let second = Identity::generate().public_id().0;
        let members = parse_members_csv(Some(&format!("{first},{second},{first}"))).unwrap();

        assert_eq!(members.len(), 2);
        assert_eq!(members[0].0, first);
        assert_eq!(members[1].0, second);
    }

    #[test]
    fn schema_file_name_sanitizes_nested_content_types() {
        assert_eq!(schema_file_name("nested/path", 2), "nested_path.v2.json");
    }
}
