//! Custom fields ("extensions") on the entities that modules declare extensible.
//!
//! A module declares its extensible entities with
//! [`Module::extensible_entities`](crate::Module::extensible_entities), e.g.
//! `masterdata.partner`. Each tenant defines fields on them, identified by
//! `<entity>` + `<key>` (e.g. `masterdata.partner` + `credit_limit`), never by
//! a server-generated id: a field is the same on Dev, QAS and PROD, so that
//! customizing can be moved between systems by transports.
//!
//! Values are stored in the entity's `ext` jsonb column. A module validates
//! them with [`ExtSchema`] on every write and filters them on read:
//!
//! ```ignore
//! let schema = ExtSchema::load(&mut tx, "masterdata.partner").await?;
//! schema.validate(&new.ext)?;            // 422 naming the field
//! /* insert */
//! partner.ext = schema.visible(partner.ext);
//! ```
//!
//! Changing a definition does not migrate stored values. Removing a field keeps
//! its values in the database, but they are no longer returned; defining the
//! field again brings them back.
//!
//! Definitions may be changed directly only on a [`SystemRole::Dev`] system.
//! QAS and PROD answer `409`: they receive customizing only through transports.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use chrono::NaiveDate;
use core_kernel::{Error, Result, TenantCtx, TenantTx, new_id};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::{CoreState, DryRun};

/// Topic published when a field is created or changed. Payload: [`FieldDefinition`].
pub const FIELD_DEFINED: &str = "core.ext_field.defined";
/// Topic published when a field is removed. Payload: [`FieldDefinition`].
pub const FIELD_DELETED: &str = "core.ext_field.deleted";

/// Longest field key, see [`check_key`].
const MAX_KEY_LEN: usize = 63;
/// Key that would clash with `GET /v1/ext-fields/{entity}/schema`.
const RESERVED_KEY: &str = "schema";

/// Which stage of a Dev → QAS → PROD landscape this server is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum SystemRole {
    /// Development: customizing may be changed directly.
    #[default]
    Dev,
    /// Quality assurance: customizing only via transport.
    Qas,
    /// Production: customizing only via transport.
    Prod,
}

impl SystemRole {
    /// Environment variable the server reads the role from.
    pub const ENV: &str = "ROSTWERK_SYSTEM_ROLE";

    /// Reads [`SystemRole::ENV`]: `dev`, `qas` or `prod`. Unset or empty is `dev`.
    pub fn from_env() -> anyhow::Result<Self> {
        match std::env::var(Self::ENV) {
            Ok(v) if !v.trim().is_empty() => {
                v.parse().map_err(|e| anyhow::anyhow!("{}: {e}", Self::ENV))
            }
            _ => Ok(Self::Dev),
        }
    }

    /// Whether customizing (e.g. field definitions) may be changed directly.
    pub fn allows_direct_customizing(self) -> bool {
        self == Self::Dev
    }
}

impl FromStr for SystemRole {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "dev" => Ok(Self::Dev),
            "qas" => Ok(Self::Qas),
            "prod" => Ok(Self::Prod),
            other => Err(format!(
                "unknown system role {other:?}, expected dev, qas or prod"
            )),
        }
    }
}

impl fmt::Display for SystemRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Dev => "dev",
            Self::Qas => "qas",
            Self::Prod => "prod",
        })
    }
}

/// An entity that tenants can add custom fields to. Returned by
/// [`Module::extensible_entities`](crate::Module::extensible_entities).
#[derive(Debug, Clone, Copy)]
pub struct ExtensibleEntity {
    /// `<module id>.<entity>`, e.g. `masterdata.partner`. Stable: it identifies
    /// the entity's fields on every system.
    pub name: &'static str,
    /// Human-readable name, e.g. `Business partner`.
    pub label: &'static str,
}

/// An extensible entity and the module that declares it.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ExtEntityInfo {
    /// `<module>.<entity>`, e.g. `masterdata.partner`.
    pub name: &'static str,
    /// Id of the declaring module.
    pub module: &'static str,
    pub label: &'static str,
}

/// All extensible entities of the registered modules, by name. Cheap to clone.
#[derive(Debug, Clone, Default)]
pub struct ExtEntities(Arc<BTreeMap<&'static str, ExtEntityInfo>>);

impl ExtEntities {
    pub(crate) fn new(entities: impl IntoIterator<Item = ExtEntityInfo>) -> Self {
        Self(Arc::new(
            entities.into_iter().map(|e| (e.name, e)).collect(),
        ))
    }

    /// The entity, if it exists and the tenant has enabled its module.
    pub fn get(&self, ctx: &TenantCtx, name: &str) -> Option<&ExtEntityInfo> {
        self.0.get(name).filter(|e| ctx.has_module(e.module))
    }

    /// The tenant's entities, ordered by name.
    pub fn visible(&self, ctx: &TenantCtx) -> Vec<ExtEntityInfo> {
        self.0
            .values()
            .filter(|e| ctx.has_module(e.module))
            .cloned()
            .collect()
    }
}

/// Checks a declared entity name: `<module>.<entity>`, lower case.
pub(crate) fn check_entity_name(module: &str, name: &str) -> bool {
    name.split_once('.').is_some_and(|(prefix, entity)| {
        prefix == module
            && entity.starts_with(|c: char| c.is_ascii_lowercase())
            && entity
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    })
}

/// Value type of a custom field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, sqlx::Type)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum FieldType {
    /// JSON string.
    String,
    /// JSON number.
    Number,
    /// Exact decimal as a string, e.g. `"1500.00"`. Use for money amounts.
    Decimal,
    /// JSON boolean.
    Boolean,
    /// Date as `YYYY-MM-DD`.
    Date,
    /// One of `enum_values`.
    Enum,
}

/// A custom field of an entity in the current tenant.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct FieldDefinition {
    /// e.g. `masterdata.partner`
    pub entity: String,
    /// e.g. `credit_limit`
    pub key: String,
    #[serde(rename = "type")]
    pub field_type: FieldType,
    pub required: bool,
    /// Allowed values, only for type `enum`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enum_values: Option<Vec<String>>,
    pub label: String,
}

/// Body of `PUT /v1/ext-fields/{entity}/{key}`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FieldSpec {
    #[serde(rename = "type")]
    pub field_type: FieldType,
    /// Default `false`.
    #[serde(default)]
    pub required: bool,
    /// Required for type `enum`, not allowed otherwise. Distinct, non-empty.
    pub enum_values: Option<Vec<String>>,
    /// Human-readable name, e.g. `Credit limit`.
    pub label: String,
}

/// Checks a field key: `^[a-z][a-z0-9_]{0,62}$`, and not `schema`.
fn check_key(key: &str) -> Result<()> {
    let valid = key.len() <= MAX_KEY_LEN
        && key.starts_with(|c: char| c.is_ascii_lowercase())
        && key
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !valid {
        return Err(Error::Validation(format!(
            "key {key:?} must match ^[a-z][a-z0-9_]{{0,62}}$"
        )));
    }
    if key == RESERVED_KEY {
        return Err(Error::Validation(format!("key {key:?} is reserved")));
    }
    Ok(())
}

impl FieldSpec {
    /// Validates and normalizes the spec into a definition of `entity`.`key`.
    fn into_definition(self, entity: &str, key: &str) -> Result<FieldDefinition> {
        check_key(key)?;
        let label = self.label.trim();
        if label.is_empty() {
            return Err(Error::Validation("label must not be empty".into()));
        }
        let enum_values = match (self.field_type, self.enum_values) {
            (FieldType::Enum, Some(values)) => {
                if values.is_empty() {
                    return Err(Error::Validation("enum_values must not be empty".into()));
                }
                for (i, v) in values.iter().enumerate() {
                    if v.is_empty() {
                        return Err(Error::Validation(
                            "enum_values must not be empty strings".into(),
                        ));
                    }
                    if values[..i].contains(v) {
                        return Err(Error::Validation(format!("enum_values has {v:?} twice")));
                    }
                }
                Some(values)
            }
            (FieldType::Enum, None) => {
                return Err(Error::Validation("type enum needs enum_values".into()));
            }
            (_, Some(_)) => {
                return Err(Error::Validation(
                    "enum_values is only allowed for type enum".into(),
                ));
            }
            (_, None) => None,
        };
        Ok(FieldDefinition {
            entity: entity.to_string(),
            key: key.to_string(),
            field_type: self.field_type,
            required: self.required,
            enum_values,
            label: label.to_string(),
        })
    }
}

/// The custom fields of one entity in the current tenant: validates and
/// filters `ext` values. See the [module docs](self).
#[derive(Debug, Clone)]
pub struct ExtSchema {
    entity: String,
    /// Ordered by key.
    fields: Vec<FieldDefinition>,
}

impl ExtSchema {
    /// Loads the definitions of `entity` in the transaction's tenant.
    pub async fn load(tx: &mut TenantTx, entity: &str) -> Result<Self> {
        let fields = sqlx::query_as!(
            FieldDefinition,
            r#"SELECT entity, key, type AS "field_type: FieldType", required, enum_values, label
             FROM ext_field_definitions
             WHERE entity = $1
             ORDER BY key"#,
            entity,
        )
        .fetch_all(tx.conn())
        .await?;
        Ok(Self {
            entity: entity.to_string(),
            fields,
        })
    }

    pub fn fields(&self) -> &[FieldDefinition] {
        &self.fields
    }

    /// Checks a complete `ext` object: no unknown keys, values of the right
    /// type, all required fields present. The error names the field, e.g.
    /// `ext.credit_limit: must be a decimal string`.
    pub fn validate(&self, ext: &Value) -> Result<()> {
        let Some(values) = ext.as_object() else {
            return Err(Error::Validation("ext must be a JSON object".into()));
        };
        if let Some(unknown) = values.keys().find(|k| self.field(k).is_none()) {
            return Err(Error::Validation(format!(
                "ext.{unknown}: unknown field of {}",
                self.entity
            )));
        }
        for field in &self.fields {
            match values.get(&field.key) {
                Some(value) => check_value(field, value)
                    .map_err(|msg| Error::Validation(format!("ext.{}: {msg}", field.key)))?,
                None if field.required => {
                    return Err(Error::Validation(format!("ext.{}: is required", field.key)));
                }
                None => {}
            }
        }
        Ok(())
    }

    /// `ext` with only the currently defined fields; values of removed fields
    /// stay stored but are not returned.
    pub fn visible(&self, ext: Value) -> Value {
        match ext {
            Value::Object(values) => Value::Object(
                values
                    .into_iter()
                    .filter(|(k, _)| self.field(k).is_some())
                    .collect(),
            ),
            other => other,
        }
    }

    /// The JSON Schema (2020-12) an `ext` object must satisfy.
    pub fn json_schema(&self) -> Value {
        let properties: Map<String, Value> = self
            .fields
            .iter()
            .map(|f| {
                let mut schema = match f.field_type {
                    FieldType::String => json!({ "type": "string" }),
                    FieldType::Number => json!({ "type": "number" }),
                    FieldType::Decimal => {
                        json!({ "type": "string", "pattern": "^-?[0-9]+(\\.[0-9]+)?$" })
                    }
                    FieldType::Boolean => json!({ "type": "boolean" }),
                    FieldType::Date => json!({ "type": "string", "format": "date" }),
                    FieldType::Enum => json!({ "type": "string", "enum": f.enum_values }),
                };
                schema["title"] = json!(f.label);
                (f.key.clone(), schema)
            })
            .collect();
        let required: Vec<&str> = self
            .fields
            .iter()
            .filter(|f| f.required)
            .map(|f| f.key.as_str())
            .collect();
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": self.entity,
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        })
    }

    fn field(&self, key: &str) -> Option<&FieldDefinition> {
        self.fields.iter().find(|f| f.key == key)
    }
}

fn check_value(field: &FieldDefinition, value: &Value) -> std::result::Result<(), String> {
    let ok = match field.field_type {
        FieldType::String => value.is_string(),
        FieldType::Number => value.is_number(),
        FieldType::Boolean => value.is_boolean(),
        FieldType::Decimal => value.as_str().is_some_and(is_decimal),
        FieldType::Date => value
            .as_str()
            .is_some_and(|s| s.len() == 10 && NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok()),
        FieldType::Enum => value
            .as_str()
            .is_some_and(|s| field.enum_values.iter().flatten().any(|v| v == s)),
    };
    if ok {
        return Ok(());
    }
    Err(match field.field_type {
        FieldType::String => "must be a string".into(),
        FieldType::Number => "must be a number".into(),
        FieldType::Boolean => "must be a boolean".into(),
        FieldType::Decimal => "must be a decimal string, e.g. \"12.50\"".into(),
        FieldType::Date => "must be a date string YYYY-MM-DD".into(),
        FieldType::Enum => format!(
            "must be one of {}",
            field
                .enum_values
                .iter()
                .flatten()
                .map(|v| format!("{v:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    })
}

/// `-?digits(.digits)?`, representable as [`Decimal`].
fn is_decimal(s: &str) -> bool {
    let digits = s.strip_prefix('-').unwrap_or(s);
    let (int, frac) = digits.split_once('.').unwrap_or((digits, "0"));
    let all_digits = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit());
    all_digits(int) && all_digits(frac) && Decimal::from_str_exact(s).is_ok()
}

// --- HTTP API

/// Core routes for custom fields. Not module-gated: entities of modules the
/// tenant has not enabled answer 404.
pub fn routes() -> OpenApiRouter<CoreState> {
    OpenApiRouter::new()
        .routes(routes!(list_entities))
        .routes(routes!(list_fields))
        .routes(routes!(schema))
        .routes(routes!(put_field, delete_field))
}

fn entity<'a>(state: &'a CoreState, ctx: &TenantCtx, name: &str) -> Result<&'a ExtEntityInfo> {
    state.ext_entities.get(ctx, name).ok_or(Error::NotFound)
}

fn check_customizing_allowed(state: &CoreState) -> Result<()> {
    if state.system_role.allows_direct_customizing() {
        return Ok(());
    }
    Err(Error::Conflict(format!(
        "this is a {} system: customizing can be changed only by importing a transport",
        state.system_role
    )))
}

/// List the entities of the tenant's modules that can have custom fields.
#[utoipa::path(
    get,
    path = "/v1/ext-entities",
    tag = "core",
    responses((status = 200, body = Vec<ExtEntityInfo>))
)]
pub async fn list_entities(
    State(state): State<CoreState>,
    ctx: TenantCtx,
) -> Json<Vec<ExtEntityInfo>> {
    Json(state.ext_entities.visible(&ctx))
}

/// List the custom fields of an entity, ordered by key.
#[utoipa::path(
    get,
    path = "/v1/ext-fields/{entity}",
    tag = "core",
    params(("entity" = String, Path, description = "e.g. `masterdata.partner`")),
    responses(
        (status = 200, body = Vec<FieldDefinition>),
        (status = 404, description = "Unknown entity")
    )
)]
pub async fn list_fields(
    State(state): State<CoreState>,
    ctx: TenantCtx,
    Path(name): Path<String>,
) -> Result<Json<Vec<FieldDefinition>>> {
    let entity = entity(&state, &ctx, &name)?;
    let mut tx = ctx.begin(&state.db).await?;
    let schema = ExtSchema::load(&mut tx, entity.name).await?;
    tx.commit().await?;
    Ok(Json(schema.fields))
}

/// The JSON Schema that the entity's `ext` object must satisfy in this tenant.
#[utoipa::path(
    get,
    path = "/v1/ext-fields/{entity}/schema",
    tag = "core",
    params(("entity" = String, Path, description = "e.g. `masterdata.partner`")),
    responses(
        (status = 200, description = "JSON Schema (2020-12)", body = Object),
        (status = 404, description = "Unknown entity")
    )
)]
pub async fn schema(
    State(state): State<CoreState>,
    ctx: TenantCtx,
    Path(name): Path<String>,
) -> Result<Json<Value>> {
    let entity = entity(&state, &ctx, &name)?;
    let mut tx = ctx.begin(&state.db).await?;
    let schema = ExtSchema::load(&mut tx, entity.name).await?;
    tx.commit().await?;
    Ok(Json(schema.json_schema()))
}

/// Create or replace a custom field. Idempotent: putting the same definition
/// again changes nothing (no audit entry, no event).
///
/// Only on Dev systems; QAS and PROD answer 409.
#[utoipa::path(
    put,
    path = "/v1/ext-fields/{entity}/{key}",
    tag = "core",
    params(
        ("entity" = String, Path, description = "e.g. `masterdata.partner`"),
        ("key" = String, Path, description = "e.g. `credit_limit`, `^[a-z][a-z0-9_]{0,62}$`"),
        DryRun
    ),
    request_body = FieldSpec,
    responses(
        (status = 201, body = FieldDefinition, description = "Created"),
        (status = 200, body = FieldDefinition, description = "Replaced or unchanged"),
        (status = 404, description = "Unknown entity"),
        (status = 409, description = "Not a Dev system: customizing only via transport"),
        (status = 422, description = "Invalid key or definition")
    )
)]
pub async fn put_field(
    State(state): State<CoreState>,
    ctx: TenantCtx,
    Path((name, key)): Path<(String, String)>,
    dry_run: DryRun,
    Json(spec): Json<FieldSpec>,
) -> Result<Response> {
    check_customizing_allowed(&state)?;
    let entity = entity(&state, &ctx, &name)?;
    let def = spec.into_definition(entity.name, &key)?;

    let mut tx = ctx.begin(&state.db).await?;
    // No row back: the stored definition is already identical.
    let written = sqlx::query_scalar!(
        r#"INSERT INTO ext_field_definitions
             (id, tenant_id, entity, key, type, required, enum_values, label)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         ON CONFLICT (tenant_id, entity, key) DO UPDATE SET
             type = EXCLUDED.type,
             required = EXCLUDED.required,
             enum_values = EXCLUDED.enum_values,
             label = EXCLUDED.label,
             updated_at = now()
         WHERE (ext_field_definitions.type, ext_field_definitions.required,
                ext_field_definitions.enum_values, ext_field_definitions.label)
               IS DISTINCT FROM
               (EXCLUDED.type, EXCLUDED.required, EXCLUDED.enum_values, EXCLUDED.label)
         RETURNING (xmax = 0) AS "inserted!""#,
        new_id(),
        tx.tenant_id().0,
        def.entity,
        def.key,
        def.field_type as FieldType,
        def.required,
        def.enum_values.as_deref(),
        def.label,
    )
    .fetch_optional(tx.conn())
    .await?;

    let status = match written {
        Some(inserted) => {
            tx.publish(FIELD_DEFINED, &def).await?;
            if inserted {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            }
        }
        None => StatusCode::OK,
    };
    dry_run.respond(tx, status, def).await
}

/// Remove a custom field. Stored values stay in the database but are no longer
/// returned. Responds with the removed definition.
///
/// Only on Dev systems; QAS and PROD answer 409.
#[utoipa::path(
    delete,
    path = "/v1/ext-fields/{entity}/{key}",
    tag = "core",
    params(
        ("entity" = String, Path, description = "e.g. `masterdata.partner`"),
        ("key" = String, Path, description = "e.g. `credit_limit`"),
        DryRun
    ),
    responses(
        (status = 200, body = FieldDefinition, description = "Removed"),
        (status = 404, description = "Unknown entity or field"),
        (status = 409, description = "Not a Dev system: customizing only via transport")
    )
)]
pub async fn delete_field(
    State(state): State<CoreState>,
    ctx: TenantCtx,
    Path((name, key)): Path<(String, String)>,
    dry_run: DryRun,
) -> Result<Response> {
    check_customizing_allowed(&state)?;
    let entity = entity(&state, &ctx, &name)?;

    let mut tx = ctx.begin(&state.db).await?;
    let def = sqlx::query_as!(
        FieldDefinition,
        r#"DELETE FROM ext_field_definitions
         WHERE entity = $1 AND key = $2
         RETURNING entity, key, type AS "field_type: FieldType", required, enum_values, label"#,
        entity.name,
        key,
    )
    .fetch_optional(tx.conn())
    .await?
    .ok_or(Error::NotFound)?;
    tx.publish(FIELD_DELETED, &def).await?;
    dry_run.respond(tx, StatusCode::OK, def).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(key: &str, field_type: FieldType, required: bool) -> FieldDefinition {
        FieldDefinition {
            entity: "test.thing".into(),
            key: key.into(),
            field_type,
            required,
            enum_values: (field_type == FieldType::Enum).then(|| vec!["a".into(), "b".into()]),
            label: key.into(),
        }
    }

    fn schema() -> ExtSchema {
        ExtSchema {
            entity: "test.thing".into(),
            fields: vec![
                field("amount", FieldType::Decimal, false),
                field("code", FieldType::String, true),
                field("count", FieldType::Number, false),
                field("due", FieldType::Date, false),
                field("flag", FieldType::Boolean, false),
                field("level", FieldType::Enum, false),
            ],
        }
    }

    fn err(ext: Value) -> String {
        match schema().validate(&ext) {
            Err(Error::Validation(msg)) => msg,
            other => panic!("expected validation error for {ext}, got {other:?}"),
        }
    }

    #[test]
    fn validates_values() {
        let s = schema();
        s.validate(&json!({ "code": "x" })).unwrap();
        s.validate(&json!({
            "code": "x", "amount": "-12.50", "count": 3.5, "due": "2026-02-28",
            "flag": true, "level": "b",
        }))
        .unwrap();

        assert_eq!(err(json!({})), "ext.code: is required");
        assert_eq!(err(json!([])), "ext must be a JSON object");
        assert_eq!(
            err(json!({ "code": "x", "nope": 1 })),
            "ext.nope: unknown field of test.thing"
        );
        assert!(err(json!({ "code": 1 })).starts_with("ext.code: must be a string"));
        assert!(err(json!({ "code": "x", "count": "3" })).starts_with("ext.count:"));
        assert!(err(json!({ "code": "x", "flag": "true" })).starts_with("ext.flag:"));
        assert!(err(json!({ "code": "x", "level": "c" })).starts_with("ext.level: must be one of"));
        for bad in [
            json!(12.5),
            json!("1e3"),
            json!("1."),
            json!(".5"),
            json!("abc"),
        ] {
            assert!(err(json!({ "code": "x", "amount": bad })).starts_with("ext.amount:"));
        }
        for bad in ["2026-02-30", "2026-2-1", "26-02-01", "2026-02-01T00:00:00Z"] {
            assert!(err(json!({ "code": "x", "due": bad })).starts_with("ext.due:"));
        }
        // Null is not "absent".
        assert!(err(json!({ "code": null })).starts_with("ext.code:"));
    }

    #[test]
    fn hides_removed_fields() {
        let ext = json!({ "code": "x", "gone": 1 });
        assert_eq!(schema().visible(ext), json!({ "code": "x" }));
    }

    #[test]
    fn json_schema_matches_definitions() {
        let js = schema().json_schema();
        assert_eq!(js["required"], json!(["code"]));
        assert_eq!(js["additionalProperties"], json!(false));
        assert_eq!(js["properties"]["due"]["format"], "date");
        assert_eq!(js["properties"]["level"]["enum"], json!(["a", "b"]));
        assert_eq!(js["properties"]["amount"]["type"], "string");
    }

    #[test]
    fn checks_keys() {
        for good in ["a", "credit_limit", "x9", &"a".repeat(63)] {
            check_key(good).unwrap();
        }
        for bad in [
            "",
            "A",
            "9a",
            "_a",
            "a-b",
            "a.b",
            "ä",
            &"a".repeat(64),
            "schema",
        ] {
            assert!(check_key(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn checks_entity_names() {
        assert!(check_entity_name("masterdata", "masterdata.partner"));
        assert!(check_entity_name("masterdata", "masterdata.sales_order2"));
        for bad in [
            "masterdata",
            "sales.order",
            "masterdata.",
            "masterdata.Partner",
            "masterdata.a.b",
        ] {
            assert!(!check_entity_name("masterdata", bad), "{bad}");
        }
    }

    #[test]
    fn parses_system_role() {
        assert_eq!("PROD".parse::<SystemRole>().unwrap(), SystemRole::Prod);
        assert_eq!(" qas ".parse::<SystemRole>().unwrap(), SystemRole::Qas);
        assert!("staging".parse::<SystemRole>().is_err());
        assert!(SystemRole::Dev.allows_direct_customizing());
        assert!(!SystemRole::Prod.allows_direct_customizing());
    }
}
