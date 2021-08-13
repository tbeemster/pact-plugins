use core::pin::Pin;
use core::task::{Context, Poll};
use std::collections::HashMap;
use std::io::Read;
use std::net::SocketAddr;

use anyhow::anyhow;
use bytes::Bytes;
use csv::{Reader, ReaderBuilder, StringRecord, Writer};
use env_logger::Env;
use futures::Stream;
use log::debug;
use maplit::hashmap;
use pact_matching::matchers::Matches;
use pact_models::bodies::OptionalBody;
use pact_models::generators::{GenerateValue, NoopVariantMatcher, VariantMatcher};
use pact_models::matchingrules::{MatchingRule, RuleList, RuleLogic};
use pact_models::prelude::{ContentType, Generator};
use prost_types::value::Kind;
use serde_json::{json, Value};
use tokio::net::{TcpListener, TcpStream};
use tonic::{Request, Response, transport::Server};
use uuid::Uuid;

use crate::parser::{parse_field, parse_value};
use crate::proto::pact_plugin_server::{PactPlugin, PactPluginServer};
use crate::proto::to_object;

mod proto;
mod parser;

#[derive(Debug, Default)]
pub struct CsvPactPlugin {}

fn setup_csv_contents(request: &Request<proto::ConfigureContentsRequest>) -> anyhow::Result<Response<proto::ConfigureContentsResponse>> {
  match &request.get_ref().contents_config {
    Some(config) => {
      let mut columns = vec![];
      for (key, value) in &config.fields {
        let column = parse_field(&key)?;
        let result = parse_value(&value)?;
        debug!("Parsed column definition: {}, {:?}", column, result);
        if column > columns.len() {
          columns.resize(column, None)
        }
        columns[column - 1] = Some(result);
      }
      let mut wtr = Writer::from_writer(vec![]);
      let column_values = columns.iter().map(|v| {
        if let Some(v) = v {
          &v.0
        } else {
          ""
        }
      }).collect::<Vec<&str>>();
      wtr.write_record(column_values)?;
      let mut rules = hashmap!{};
      let mut generators = hashmap!{};
      for (col, vals) in columns.iter().enumerate() {
        if let Some((_, rule, gen)) = vals {
          if let Some(rule) = rule {
            debug!("rule.values()={:?}", rule.values());
            rules.insert(format!("column:{}", col), proto::MatchingRules {
              rule: vec![
                proto::MatchingRule {
                  r#type: rule.name(),
                  values: Some(prost_types::Struct {
                    fields: rule.values().iter().map(|(key, val)| (key.to_string(), to_value(val))).collect()
                  })
                }
              ]
            });
          }
          if let Some(gen) = gen {
            generators.insert(format!("column:{}", col), proto::Generator {
              r#type: gen.name(),
              values: Some(prost_types::Struct {
                fields: gen.values().iter().map(|(key, val)| (key.to_string(), to_value(val))).collect()
              })
            });
          }
        }
      }
      debug!("matching rules = {:?}", rules);
      debug!("generators = {:?}", generators);
      Ok(Response::new(proto::ConfigureContentsResponse {
        contents: Some(proto::Body {
          content_type: "text/csv;charset=UTF-8".to_string(),
          content: Some(wtr.into_inner()?),
        }),
        rules,
        generators
      }))
    }
    None => Err(anyhow!("No config provided to match/generate CSV content"))
  }
}

fn generate_csv_content(request: &Request<proto::GenerateContentRequest>) -> anyhow::Result<OptionalBody> {
  let mut generators = hashmap! {};
  for (key, gen) in &request.get_ref().generators {
    let column = parse_field(&key)?;
    let values = gen.values.as_ref().ok_or(anyhow!("Generator values were expected"))?.fields.iter().map(|(k, v)| {
      (k.clone(), from_value(v))
    }).collect();
    let generator = Generator::from_map(&gen.r#type, &values)
      .ok_or(anyhow!("Failed to build generator of type {}", gen.r#type))?;
    generators.insert(column, generator);
  };

  let context = hashmap! {};
  let variant_matcher = NoopVariantMatcher.boxed();
  let mut wtr = Writer::from_writer(vec![]);
  let csv_data = request.get_ref().contents.as_ref().unwrap().content.as_ref().unwrap();
  let mut rdr = ReaderBuilder::new().has_headers(false).from_reader(csv_data.as_slice());
  for result in rdr.records() {
    let record = result?;
    for (col, field) in record.iter().enumerate() {
      debug!("got column:{} = '{}'", col, field);
      if let Some(generator) = generators.get(&col) {
        let value = generator.generate_value(&field.to_string(), &context, &variant_matcher)?;
        wtr.write_field(value)?;
      } else {
        wtr.write_field(field)?;
      }
    }
    wtr.write_record(None::<&[u8]>)?;
  }
  let generated = wtr.into_inner()?;
  debug!("Generated contents has {} bytes", generated.len());
  let bytes = Bytes::from(generated);
  Ok(OptionalBody::Present(bytes, Some(ContentType::from("text/csv;charset=UTF-8"))))
}

fn to_value(value: &Value) -> prost_types::Value {
  match value {
    Value::Null => prost_types::Value { kind: Some(prost_types::value::Kind::NullValue(0)) },
    Value::Bool(b) => prost_types::Value { kind: Some(prost_types::value::Kind::BoolValue(*b)) },
    Value::Number(n) => if n.is_u64() {
      prost_types::Value { kind: Some(prost_types::value::Kind::NumberValue(n.as_u64().unwrap_or_default() as f64)) }
    } else if n.is_i64() {
      prost_types::Value { kind: Some(prost_types::value::Kind::NumberValue(n.as_i64().unwrap_or_default() as f64)) }
    } else {
      prost_types::Value { kind: Some(prost_types::value::Kind::NumberValue(n.as_f64().unwrap_or_default())) }
    }
    Value::String(s) => prost_types::Value { kind: Some(prost_types::value::Kind::StringValue(s.clone())) },
    Value::Array(v) => prost_types::Value { kind: Some(prost_types::value::Kind::ListValue(prost_types::ListValue {
      values: v.iter().map(|val| to_value(val)).collect()
    })) },
    Value::Object(m) => prost_types::Value { kind: Some(prost_types::value::Kind::StructValue(prost_types::Struct {
      fields: m.iter().map(|(key, val)| (key.clone(), to_value(val))).collect()
    })) }
  }
}

fn from_value(value: &prost_types::Value) -> Value {
  match value.kind.as_ref().unwrap() {
    Kind::NullValue(_) => Value::Null,
    Kind::NumberValue(n) => json!(*n),
    Kind::StringValue(s) => Value::String(s.clone()),
    Kind::BoolValue(b) => Value::Bool(*b),
    Kind::StructValue(s) => Value::Object(s.fields.iter()
      .map(|(k, v)| (k.clone(), from_value(v))).collect()),
    Kind::ListValue(l) => Value::Array(l.values.iter()
      .map(|v| from_value(v)).collect())
  }
}

#[tonic::async_trait]
impl PactPlugin for CsvPactPlugin {
  async fn init_plugin(
    &self,
    request: tonic::Request<proto::InitPluginRequest>,
  ) -> Result<tonic::Response<proto::InitPluginResponse>, tonic::Status> {
    let message = request.get_ref();
    debug!("Init request from {}/{}", message.implementation, message.version);
    Ok(Response::new(proto::InitPluginResponse {
      catalogue: vec![
        proto::CatalogueEntry {
          r#type: "content-matcher".to_string(),
          key: "csv".to_string(),
          values: hashmap! {
            "content-types".to_string() => "text/csv;application/csv".to_string()
          }
        },
        proto::CatalogueEntry {
          r#type: "content-generator".to_string(),
          key: "csv".to_string(),
          values: hashmap! {
            "content-types".to_string() => "text/csv;application/csv".to_string()
          }
        }
      ]
    }))
  }

  async fn update_catalogue(
    &self,
    _request: tonic::Request<proto::Catalogue>,
  ) -> Result<tonic::Response<proto::Void>, tonic::Status> {
    debug!("Update catalogue request, ignoring");
    Ok(Response::new(proto::Void {}))
  }

  async fn compare_contents(
    &self,
    request: tonic::Request<proto::CompareContentsRequest>,
  ) -> Result<tonic::Response<proto::CompareContentsResponse>, tonic::Status> {
    debug!("compare_contents request");
    let request = request.get_ref();
    match (request.expected.as_ref(), request.actual.as_ref()) {
      (Some(expected), Some(actual)) => {
        let expected_csv_data = expected.content.as_ref().unwrap();
        let mut expected_rdr = ReaderBuilder::new().has_headers(false)
          .from_reader(expected_csv_data.as_slice());
        let actual_csv_data = actual.content.as_ref().unwrap();
        let mut actual_rdr = ReaderBuilder::new().has_headers(false)
          .from_reader(actual_csv_data.as_slice());
        let rules = request.rules.iter()
          .map(|(key, rules)| {
            let rules = rules.rule.iter().fold(RuleList::empty(RuleLogic::And), |mut list, rule| {
              match to_object(&rule.values.as_ref().unwrap()) {
                Value::Object(mut map) => {
                  map.insert("match".to_string(), Value::String(rule.r#type.clone()));
                  debug!("Creating matching rule with {:?}", map);
                  list.add_rule(&MatchingRule::from_json(&Value::Object(map)).unwrap());
                }
                _ => {}
              }
              list
            });
            (key.clone(), rules)
          }).collect();
        compare_contents(&mut expected_rdr, &mut actual_rdr, request.allow_unexpected_keys, rules)
          .map_err(|err| tonic::Status::aborted(format!("Failed to compare CSV contents: {}", err)))
      }
      (None, Some(actual)) => {
        let contents = actual.content.as_ref().unwrap();
        Ok(Response::new(proto::CompareContentsResponse {
          type_mismatch: None,
          results: vec![
            proto::ContentMismatch {
              expected: None,
              actual: Some(contents.clone()),
              mismatch: format!("Expected no CSV content, but got {} bytes", contents.len()),
              path: "".to_string(),
              diff: "".to_string()
            }
          ]
        }))
      }
      (Some(expected), None) => {
        let contents = expected.content.as_ref().unwrap();
        Ok(Response::new(proto::CompareContentsResponse {
          type_mismatch: None,
          results: vec![
            proto::ContentMismatch {
              expected: Some(contents.clone()),
              actual: None,
              mismatch: format!("Expected CSV content, but did not get any"),
              path: "".to_string(),
              diff: "".to_string()
            }
          ]
        }))
      }
      (None, None) => {
        Ok(Response::new(proto::CompareContentsResponse {
          type_mismatch: None,
          results: vec![]
        }))
      }
    }
  }

  async fn configure_contents(
    &self,
    request: tonic::Request<proto::ConfigureContentsRequest>,
  ) -> Result<tonic::Response<proto::ConfigureContentsResponse>, tonic::Status> {
    debug!("Received configure_contents request for '{}'", request.get_ref().content_type);

    // "column:1", "matching(type,'Name')",
    // "column:2", "matching(number,100)",
    // "column:3", "matching(datetime, 'yyyy-MM-dd','2000-01-01')"
    setup_csv_contents(&request)
      .map_err(|err| tonic::Status::aborted(format!("Invalid column definition: {}", err)))
  }

  async fn generate_content(
    &self,
    request: tonic::Request<proto::GenerateContentRequest>,
  ) -> Result<tonic::Response<proto::GenerateContentResponse>, tonic::Status> {
    debug!("Received generate_content request");

    generate_csv_content(&request)
      .map(|contents| {
        debug!("Generated contents: {}", contents);
        Response::new(proto::GenerateContentResponse {
          contents: Some(proto::Body {
            content_type: contents.content_type().unwrap_or(ContentType::from("text/csv")).to_string(),
            content: Some(contents.value().unwrap().to_vec()),
          })
        })
      })
      .map_err(|err| tonic::Status::aborted(format!("Failed to generate CSV contents: {}", err)))
  }
}

fn compare_contents<R: Read>(
  expected: &mut Reader<R>,
  actual: &mut Reader<R>,
  allow_unexpected_keys: bool,
  rules: HashMap<String, RuleList>
) -> anyhow::Result<tonic::Response<proto::CompareContentsResponse>> {
  debug!("Comparing contents using allow_unexpected_keys ({}) and rules ({:?})", allow_unexpected_keys, rules);

  let mut expected_records = expected.records();
  let mut actual_records = actual.records();
  let mut results = vec![];

  let expected_row = expected_records.next()
    .ok_or_else(|| anyhow!("Could not read the expected content"))??;
  let actual_row = actual_records.next()
    .ok_or_else(|| anyhow!("Could not read the expected content"))??;
  if actual_row.len() < expected_row.len() {
    results.push(proto::ContentMismatch {
      expected: Some(format!("{} columns", expected_row.len()).as_bytes().to_vec()),
      actual: Some(format!("{} columns", actual_row.len()).as_bytes().to_vec()),
      mismatch: format!("Expected {} columns, but got {}", expected_row.len(), actual_row.len()),
      path: String::default(),
      diff: String::default()
    });
  } else if actual_row.len() > expected_row.len() && !allow_unexpected_keys {
    results.push(proto::ContentMismatch {
      expected: Some(format!("{} columns", expected_row.len()).as_bytes().to_vec()),
      actual: Some(format!("{} columns", actual_row.len()).as_bytes().to_vec()),
      mismatch: format!("Expected at least {} columns, but got {}", expected_row.len(), actual_row.len()),
      path: String::default(),
      diff: String::default()
    });
  }

  compare_row(&expected_row, &actual_row, &rules, &mut results);
  for row in actual_records {
    compare_row(&expected_row, &row?, &rules, &mut results);
  }

  Ok(Response::new(proto::CompareContentsResponse {
    type_mismatch: None,
    results
  }))
}

fn compare_row(
  expected_row: &StringRecord,
  actual_row: &StringRecord,
  rules: &HashMap<String, RuleList>,
  results: &mut Vec<proto::ContentMismatch>) {
  for (index, item) in actual_row.iter().enumerate() {
    let expected_item = expected_row.get(index).unwrap_or_default();
    let path = format!("column:{}", index);
    if let Some(rules) = rules.get(&path) {
      for rule in &rules.rules {
        if let Err(err) = expected_item.matches_with(item, rule, false) {
          results.push(proto::ContentMismatch {
            expected: Some(expected_item.as_bytes().to_vec()),
            actual: Some(item.as_bytes().to_vec()),
            mismatch: err.to_string(),
            path: format!("row:{:5}, column:{:2}", actual_row.position().unwrap().line(), index),
            diff: String::default()
          });
        }
      }
    } else if item != expected_item {
      results.push(proto::ContentMismatch {
        expected: Some(expected_item.as_bytes().to_vec()),
        actual: Some(item.as_bytes().to_vec()),
        mismatch: format!("Expected column {} value to equal '{}', but got '{}'", index, expected_item, item),
        path: format!("row:{:5}, column:{:2}", actual_row.position().unwrap().line(), index),
        diff: String::default()
      });
    }
  }
}

struct TcpIncoming {
  inner: TcpListener
}

impl Stream for TcpIncoming {
  type Item = Result<TcpStream, std::io::Error>;

  fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
    Pin::new(&mut self.inner).poll_accept(cx)
      .map_ok(|(stream, _)| stream).map(|v| Some(v))
  }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
  let env = Env::new().filter("LOG_LEVEL");
  env_logger::init_from_env(env);

  let addr: SocketAddr = "0.0.0.0:0".parse()?;
  let listener = TcpListener::bind(addr).await?;
  let address = listener.local_addr()?;

  let server_key = Uuid::new_v4().to_string();
  println!("{{\"port\":{}, \"serverKey\":\"{}\"}}", address.port(), server_key);

  let plugin = CsvPactPlugin::default();
  Server::builder()
    .add_service(PactPluginServer::new(plugin))
    .serve_with_incoming(TcpIncoming { inner: listener }).await?;

  Ok(())
}
