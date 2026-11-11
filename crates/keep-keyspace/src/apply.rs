//! Execute one [`WalOp`] against a [`KvEngine`] and capture its result.
//!
//! WAL recovery ([`crate::persistence::recovery`]) replays ops and discards
//! their results. A replicated write needs the opposite: the raft state machine
//! runs the op exactly once per node, in log order, and the proposing handler
//! answers from **that** result rather than from a racy local read. This module
//! is the shared executor; the outcome travels back through raft-runtime's
//! `OutcomeWindow`.

use crate::engine::KvEngine;
use crate::error::KvError;
use crate::persistence::format::WalOp;
use crate::types::{KvKey, KvValue};

/// What applying one [`WalOp`] produced, in the shape the HTTP handler needs.
#[derive(Debug, Clone, PartialEq)]
pub enum ApplyOutcome {
    /// Write with no result (`SET`, `MSET`).
    Done,
    /// `DEL`, `CAS`, `SETNX`, lock acquire / release / extend.
    Bool(bool),
    /// `INCR`/`DECR`/`HINCRBY` value; `EXPIRE`/`PEXPIRE`/`PERSIST` status.
    Int(i64),
    /// Element counts (`MDEL`, hash / set / zset adds and removes, list length).
    Count(usize),
    /// `HINCRBYFLOAT` / `ZINCRBY` value.
    Float(f64),
    /// Popped / read value (`LPOP`, `RPOP`, `GETEX`).
    Value(Option<KvValue>),
}

/// Result of applying one op: the domain outcome, or the engine's refusal.
pub type ApplyResult = Result<ApplyOutcome, KvError>;

impl ApplyOutcome {
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            ApplyOutcome::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_int(&self) -> Option<i64> {
        match self {
            ApplyOutcome::Int(v) => Some(*v),
            _ => None,
        }
    }

    pub fn as_count(&self) -> Option<usize> {
        match self {
            ApplyOutcome::Count(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_float(&self) -> Option<f64> {
        match self {
            ApplyOutcome::Float(v) => Some(*v),
            _ => None,
        }
    }

    pub fn into_value(self) -> Option<Option<KvValue>> {
        match self {
            ApplyOutcome::Value(v) => Some(v),
            _ => None,
        }
    }
}

/// Run `op` through the engine's public API and return its result.
pub fn execute(engine: &KvEngine, op: WalOp) -> ApplyResult {
    use ApplyOutcome::*;
    Ok(match op {
        WalOp::Set { key, value, ttl } => {
            engine.set(&KvKey::new(key)?, value, ttl)?;
            Done
        }
        WalOp::Delete { key } => Bool(engine.delete(&KvKey::new(key)?)),
        WalOp::Incr { key, delta } => Int(engine.incr(&KvKey::new(key)?, delta)?),
        WalOp::Decr { key, delta } => Int(engine.decr(&KvKey::new(key)?, delta)?),
        WalOp::MSet { pairs, ttl } => {
            let keys = pairs
                .iter()
                .map(|(k, _)| KvKey::new(k.as_str()))
                .collect::<Result<Vec<_>, _>>()?;
            let pairs: Vec<(&KvKey, KvValue)> =
                keys.iter().zip(pairs.into_iter().map(|(_, v)| v)).collect();
            engine.mset(&pairs, ttl)?;
            Done
        }
        WalOp::MDel { keys } => {
            let keys = keys
                .into_iter()
                .map(KvKey::new)
                .collect::<Result<Vec<_>, _>>()?;
            let refs: Vec<&KvKey> = keys.iter().collect();
            Count(engine.mdel(&refs))
        }
        WalOp::SetNx { key, value, ttl } => Bool(engine.setnx(&KvKey::new(key)?, value, ttl)?),
        WalOp::Lock { key, owner, ttl } => Bool(engine.lock(&KvKey::new(key)?, &owner, ttl)),
        WalOp::Unlock { key, owner } => Bool(engine.unlock(&KvKey::new(key)?, &owner)?),
        WalOp::ExtendLock { key, owner, ttl } => {
            Bool(engine.extend_lock(&KvKey::new(key)?, &owner, ttl)?)
        }
        WalOp::Cas {
            key,
            expected,
            new,
            ttl,
        } => Bool(engine.cas(&KvKey::new(key)?, &expected, new, ttl)?),
        WalOp::Expire { key, seconds } => Int(engine.expire(&KvKey::new(key)?, seconds) as i64),
        WalOp::PExpire { key, milliseconds } => {
            Int(engine.pexpire(&KvKey::new(key)?, milliseconds) as i64)
        }
        WalOp::Persist { key } => Int(engine.persist(&KvKey::new(key)?) as i64),
        WalOp::HSet { key, fields } => Count(engine.hset(&KvKey::new(key)?, fields)?),
        WalOp::HDel { key, fields } => {
            let refs: Vec<&str> = fields.iter().map(String::as_str).collect();
            Count(engine.hdel(&KvKey::new(key)?, &refs)?)
        }
        WalOp::HIncrBy { key, field, delta } => {
            Int(engine.hincrby(&KvKey::new(key)?, &field, delta)?)
        }
        WalOp::HIncrByFloat { key, field, delta } => {
            Float(engine.hincrbyfloat(&KvKey::new(key)?, &field, delta)?)
        }
        WalOp::SAdd { key, members } => Count(engine.sadd(&KvKey::new(key)?, members)?),
        WalOp::SRem { key, members } => Count(engine.srem(&KvKey::new(key)?, members)?),
        WalOp::ZAdd { key, members } => Count(engine.zadd(&KvKey::new(key)?, members)?),
        WalOp::ZRem { key, members } => Count(engine.zrem(&KvKey::new(key)?, members)?),
        WalOp::ZIncrBy { key, member, delta } => {
            Float(engine.zincrby(&KvKey::new(key)?, &member, delta)?)
        }
        WalOp::LPush { key, values } => Count(engine.lpush(&KvKey::new(key)?, values)?),
        WalOp::RPush { key, values } => Count(engine.rpush(&KvKey::new(key)?, values)?),
        WalOp::LPop { key } => Value(engine.lpop(&KvKey::new(key)?)),
        WalOp::RPop { key } => Value(engine.rpop(&KvKey::new(key)?)),
        WalOp::GetEx {
            key,
            ttl_ms,
            persist,
        } => Value(engine.getex(
            &KvKey::new(key)?,
            ttl_ms.map(std::time::Duration::from_millis),
            persist,
        )),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op_set(key: &str, v: i64) -> WalOp {
        WalOp::Set {
            key: key.into(),
            value: KvValue::Int(v),
            ttl: None,
        }
    }

    #[test]
    fn execute_reports_each_ops_own_result() {
        let e = KvEngine::new();
        assert_eq!(execute(&e, op_set("a", 1)).unwrap(), ApplyOutcome::Done);
        let incr = WalOp::Incr {
            key: "a".into(),
            delta: 4,
        };
        assert_eq!(execute(&e, incr).unwrap(), ApplyOutcome::Int(5));
        let cas = |expected: i64| WalOp::Cas {
            key: "a".into(),
            expected: KvValue::Int(expected),
            new: KvValue::Int(9),
            ttl: None,
        };
        assert_eq!(execute(&e, cas(1)).unwrap(), ApplyOutcome::Bool(false));
        assert_eq!(execute(&e, cas(5)).unwrap(), ApplyOutcome::Bool(true));
        let setnx = WalOp::SetNx {
            key: "a".into(),
            value: KvValue::Int(0),
            ttl: None,
        };
        assert_eq!(execute(&e, setnx).unwrap(), ApplyOutcome::Bool(false));
        let del = || WalOp::Delete { key: "a".into() };
        assert_eq!(execute(&e, del()).unwrap(), ApplyOutcome::Bool(true));
        assert_eq!(execute(&e, del()).unwrap(), ApplyOutcome::Bool(false));
        let push = WalOp::RPush {
            key: "l".into(),
            values: vec![KvValue::Int(1), KvValue::Int(2)],
        };
        assert_eq!(execute(&e, push).unwrap(), ApplyOutcome::Count(2));
        let pop = WalOp::LPop { key: "l".into() };
        assert_eq!(
            execute(&e, pop).unwrap(),
            ApplyOutcome::Value(Some(KvValue::Int(1)))
        );
    }

    #[test]
    fn execute_surfaces_engine_errors() {
        let e = KvEngine::new();
        execute(
            &e,
            WalOp::Set {
                key: "s".into(),
                value: KvValue::String("x".into()),
                ttl: None,
            },
        )
        .unwrap();
        let incr = WalOp::Incr {
            key: "s".into(),
            delta: 1,
        };
        assert!(matches!(
            execute(&e, incr),
            Err(KvError::TypeMismatch { .. })
        ));
        let cas_missing = WalOp::Cas {
            key: "missing".into(),
            expected: KvValue::Int(0),
            new: KvValue::Int(1),
            ttl: None,
        };
        assert!(matches!(
            execute(&e, cas_missing),
            Err(KvError::KeyNotFound(_))
        ));
    }
}
