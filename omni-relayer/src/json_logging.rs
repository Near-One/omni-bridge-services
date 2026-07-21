//! JSON stdout logging: one flat object per event, shipped by the cluster's log
//! agent (Grafana Alloy / GCP Cloud Logging). Fields from enclosing spans are
//! merged in at the top level alongside the event's own fields, so they query
//! the same way as `transfer_id`. Emits both `severity` (GCP Cloud Logging maps
//! this to the entry severity) and `level` (for Loki/humans).

use std::io::Write as _;

use chrono::Utc;
use serde_json::{Map, Value};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

/// Records `tracing` fields into a JSON object.
struct JsonVisitor<'a>(&'a mut Map<String, Value>);

impl Visit for JsonVisitor<'_> {
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.0.insert(field.name().to_owned(), value.into());
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.0.insert(field.name().to_owned(), value.into());
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0.insert(field.name().to_owned(), value.into());
    }
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.0.insert(field.name().to_owned(), value.into());
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_owned(), value.into());
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().to_owned(), format!("{value:?}").into());
    }
}

/// A span's own fields, captured as JSON at creation, to be flattened onto every
/// event emitted while the span is entered.
struct SpanFields(Map<String, Value>);

/// GCP Cloud Logging severity names, which differ from tracing's level names
/// (e.g. `WARN` -> `WARNING`; GCP has no `TRACE`).
fn gcp_severity(level: Level) -> &'static str {
    match level {
        Level::ERROR => "ERROR",
        Level::WARN => "WARNING",
        Level::INFO => "INFO",
        Level::DEBUG | Level::TRACE => "DEBUG",
    }
}

pub struct JsonLogger;

impl<S> Layer<S> for JsonLogger
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let mut fields = Map::new();
        attrs.record(&mut JsonVisitor(&mut fields));
        span.extensions_mut().insert(SpanFields(fields));
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let mut ext = span.extensions_mut();
        if let Some(SpanFields(fields)) = ext.get_mut::<SpanFields>() {
            values.record(&mut JsonVisitor(fields));
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let meta = event.metadata();
        let mut obj = Map::new();
        obj.insert("timestamp".to_owned(), Utc::now().to_rfc3339().into());
        obj.insert("level".to_owned(), meta.level().as_str().into());
        obj.insert("severity".to_owned(), gcp_severity(*meta.level()).into());
        obj.insert("target".to_owned(), meta.target().into());

        // Flatten enclosing span fields outermost-first, so inner spans — and then
        // the event's own fields — win on a name clash.
        if let Some(scope) = ctx.event_scope(event) {
            for span in scope.from_root() {
                if let Some(SpanFields(fields)) = span.extensions().get::<SpanFields>() {
                    obj.extend(fields.clone());
                }
            }
        }
        event.record(&mut JsonVisitor(&mut obj));

        let mut line = serde_json::to_string(&Value::Object(obj)).unwrap_or_default();
        line.push('\n');
        let _ = std::io::stdout().lock().write_all(line.as_bytes());
    }
}
