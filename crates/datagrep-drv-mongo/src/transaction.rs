//! [`MongoTransaction`]: an explicit, `begin()`-opened transaction (design
//! §3.1's `Transaction` trait; §3.5's session-pinning rule).
//!
//! **Why no actor task, unlike `datagrep-drv-postgres`'s `actor.rs`.** The
//! ticket suggested reusing the Postgres actor-task pattern "if the
//! mongodb driver's `Cursor` needs it" — it turns out it doesn't.
//! `tokio_postgres::Transaction<'a>` *borrows* `&'a mut Client`, which is
//! incompatible with a `'static` `Box<dyn Cursor>` and forces the whole
//! transaction to live inside one task. `mongodb::ClientSession` is owned
//! (not borrowed from `Client`), so it can simply live behind an
//! `Arc<tokio::sync::Mutex<ClientSession>>` shared between this type and
//! every [`crate::cursor::MongoCursor::session`] it hands out, each of which
//! reacquires the lock only for the moment of an `advance()` call. No task,
//! no channel, no borrow-checker fight.
//!
//! **v1 scope.** Only `Op::Mutate` (the primary reason to open a
//! transaction: atomic multi-document writes) and a `find`/insert/update/
//! delete subset of `Request::Native` shell text run inside an explicit
//! transaction; `aggregate`, raw commands, and `EXPLAIN` are refused with
//! `DbError::Unsupported` rather than silently running outside it — see the
//! crate report's deviations.

use std::sync::Arc;

use async_trait::async_trait;
use bson::{doc, Document as BsonDocument};
use tokio::sync::Mutex;

use datagrep_api::driver::{Cursor, Transaction};
use datagrep_api::error::DbError;
use datagrep_api::request::{DdlOp, Mutation, MutationBatch, Op, Request};
use datagrep_api::value::Value;

use datagrep_lang::mongo::{parse, MongoStatement, ParsedMongo};

use crate::connection::{arg_doc, as_bson_doc, map_parse_err, resolve_object_path};
use crate::cursor::{AckCursor, MongoCursor, ResumeStrategy};
use crate::error::map_mongo_error;
use crate::filter;
use crate::value::value_to_bson_for_field;

pub struct MongoTransaction {
    client: mongodb::Client,
    default_database: String,
    session: Arc<Mutex<mongodb::ClientSession>>,
}

impl MongoTransaction {
    pub fn new(
        client: mongodb::Client,
        default_database: String,
        session: mongodb::ClientSession,
    ) -> Self {
        Self {
            client,
            default_database,
            session: Arc::new(Mutex::new(session)),
        }
    }

    fn collection(&self, db: &str, coll: &str) -> mongodb::Collection<BsonDocument> {
        self.client.database(db).collection::<BsonDocument>(coll)
    }

    async fn execute_text(&self, text: &str) -> Result<Box<dyn Cursor>, DbError> {
        match parse(text).map_err(map_parse_err)? {
            ParsedMongo::Chain(stmt) => self.dispatch_method(stmt).await,
            ParsedMongo::RawCommand(_) => Err(DbError::Unsupported {
                feature: "raw command documents are not supported inside an explicit transaction — use db.<collection>.<method>(...) instead".into(),
            }),
        }
    }

    async fn dispatch_method(&self, stmt: MongoStatement) -> Result<Box<dyn Cursor>, DbError> {
        let method = stmt.method.to_ascii_lowercase();
        let coll = self.collection(&self.default_database, &stmt.collection);
        match method.as_str() {
            "find" => self.run_find(coll, &stmt).await,
            "insertone" => self.run_insert_one(coll, &stmt).await,
            "updateone" => self.run_update(coll, &stmt, false).await,
            "updatemany" => self.run_update(coll, &stmt, true).await,
            "deleteone" => self.run_delete(coll, &stmt, false).await,
            "deletemany" => self.run_delete(coll, &stmt, true).await,
            other => Err(DbError::Unsupported {
                feature: format!(
                    "db.<collection>.{other}(...) is not supported inside an explicit transaction (v1 scope: find/insertOne/updateOne/updateMany/deleteOne/deleteMany)"
                ),
            }),
        }
    }

    async fn run_find(
        &self,
        coll: mongodb::Collection<BsonDocument>,
        stmt: &MongoStatement,
    ) -> Result<Box<dyn Cursor>, DbError> {
        let filter_doc = arg_doc(&stmt.args, 0)?;
        let mut session = self.session.lock().await;
        let cursor = coll
            .find(filter_doc)
            .session(&mut *session)
            .await
            .map_err(map_mongo_error)?;
        drop(session);
        Ok(Box::new(MongoCursor::session(
            cursor,
            self.session.clone(),
            ResumeStrategy::None,
        )))
    }

    async fn run_insert_one(
        &self,
        coll: mongodb::Collection<BsonDocument>,
        stmt: &MongoStatement,
    ) -> Result<Box<dyn Cursor>, DbError> {
        let bson_doc = arg_doc(&stmt.args, 0)?;
        let mut session = self.session.lock().await;
        coll.insert_one(bson_doc)
            .session(&mut *session)
            .await
            .map_err(map_mongo_error)?;
        Ok(Box::new(AckCursor::new(
            Some(1),
            Some(Arc::from("insert_one")),
        )))
    }

    async fn run_update(
        &self,
        coll: mongodb::Collection<BsonDocument>,
        stmt: &MongoStatement,
        many: bool,
    ) -> Result<Box<dyn Cursor>, DbError> {
        let filter_doc = arg_doc(&stmt.args, 0)?;
        let update_doc = arg_doc(&stmt.args, 1)?;
        let mut session = self.session.lock().await;
        let result = if many {
            coll.update_many(filter_doc, update_doc)
                .session(&mut *session)
                .await
        } else {
            coll.update_one(filter_doc, update_doc)
                .session(&mut *session)
                .await
        }
        .map_err(map_mongo_error)?;
        Ok(Box::new(AckCursor::new(
            Some(result.modified_count),
            Some(Arc::from(format!(
                "matched {}, modified {}",
                result.matched_count, result.modified_count
            ))),
        )))
    }

    async fn run_delete(
        &self,
        coll: mongodb::Collection<BsonDocument>,
        stmt: &MongoStatement,
        many: bool,
    ) -> Result<Box<dyn Cursor>, DbError> {
        let filter_doc = arg_doc(&stmt.args, 0)?;
        let mut session = self.session.lock().await;
        let result = if many {
            coll.delete_many(filter_doc).session(&mut *session).await
        } else {
            coll.delete_one(filter_doc).session(&mut *session).await
        }
        .map_err(map_mongo_error)?;
        Ok(Box::new(AckCursor::new(
            Some(result.deleted_count),
            Some(Arc::from("delete")),
        )))
    }

    async fn execute_mutate(&self, batch: &MutationBatch) -> Result<Box<dyn Cursor>, DbError> {
        let mut total = 0u64;
        for m in &batch.mutations {
            total += self.execute_one_mutation(m).await?;
        }
        Ok(Box::new(AckCursor::new(Some(total), None)))
    }

    async fn execute_one_mutation(&self, m: &Mutation) -> Result<u64, DbError> {
        match m {
            Mutation::Insert { path, doc } => {
                let (db, coll_name) = resolve_object_path(&self.default_database, path)?;
                let coll = self.collection(&db, &coll_name);
                let bson_doc = as_bson_doc(doc)?;
                let mut session = self.session.lock().await;
                coll.insert_one(bson_doc)
                    .session(&mut *session)
                    .await
                    .map_err(map_mongo_error)?;
                Ok(1)
            }
            Mutation::Update { path, key, sets } => {
                let (db, coll_name) = resolve_object_path(&self.default_database, path)?;
                let coll = self.collection(&db, &coll_name);
                let id_filter = id_filter(key)?;
                let mut set_doc = BsonDocument::new();
                for (field, value) in sets {
                    let f = filter::field_path_to_mongo(field);
                    set_doc.insert(f.clone(), value_to_bson_for_field(&f, value)?);
                }
                let mut session = self.session.lock().await;
                let result = coll
                    .update_one(id_filter, doc! { "$set": set_doc })
                    .session(&mut *session)
                    .await
                    .map_err(map_mongo_error)?;
                if result.matched_count != 1 {
                    return Err(DbError::Query {
                        code: None,
                        message: format!(
                            "row identity changed — refresh (expected exactly 1 document matched, got {})",
                            result.matched_count
                        ),
                        position: None,
                    });
                }
                Ok(1)
            }
            Mutation::Delete { path, key } => {
                let (db, coll_name) = resolve_object_path(&self.default_database, path)?;
                let coll = self.collection(&db, &coll_name);
                let id_filter_doc = id_filter(key)?;
                let mut session = self.session.lock().await;
                let result = coll
                    .delete_one(id_filter_doc)
                    .session(&mut *session)
                    .await
                    .map_err(map_mongo_error)?;
                if result.deleted_count != 1 {
                    return Err(DbError::Query {
                        code: None,
                        message: format!(
                            "row identity changed — refresh (expected exactly 1 document deleted, got {})",
                            result.deleted_count
                        ),
                        position: None,
                    });
                }
                Ok(1)
            }
        }
    }
}

fn id_filter(key: &[Value]) -> Result<BsonDocument, DbError> {
    match key {
        [only] => Ok(doc! { "_id": value_to_bson_for_field("_id", only)? }),
        other => Err(DbError::Unsupported {
            feature: format!(
                "MongoDB row identity is always a single _id value; got {} key value(s)",
                other.len()
            ),
        }),
    }
}

#[async_trait]
impl Transaction for MongoTransaction {
    async fn execute(&self, req: Request) -> Result<Box<dyn Cursor>, DbError> {
        match req {
            Request::Native { text, params, .. } => {
                if !params.is_empty() {
                    return Err(DbError::Unsupported {
                        feature: "MongoDB native shell text takes no bind parameters".into(),
                    });
                }
                self.execute_text(&text).await
            }
            Request::Op(Op::Mutate(batch)) => self.execute_mutate(&batch).await,
            Request::Op(Op::Ddl(DdlOp::Native { text })) => self.execute_text(&text).await,
            other => Err(DbError::Unsupported {
                feature: format!(
                    "request shape not supported inside an explicit MongoDB transaction (v1 scope: Native find/insert/update/delete and Op::Mutate): {other:?}"
                ),
            }),
        }
    }

    async fn commit(self: Box<Self>) -> Result<(), DbError> {
        let mut session = self.session.lock().await;
        session.commit_transaction().await.map_err(map_mongo_error)
    }

    async fn rollback(self: Box<Self>) -> Result<(), DbError> {
        let mut session = self.session.lock().await;
        session.abort_transaction().await.map_err(map_mongo_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_filter_requires_exactly_one_key_value() {
        assert!(id_filter(&[Value::I64(1), Value::I64(2)]).is_err());
        assert!(id_filter(&[]).is_err());
        assert!(id_filter(&[Value::I64(1)]).is_ok());
    }
}
