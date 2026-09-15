use async_trait::async_trait;
use eliteforge_agent_framework::*;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
#[derive(Debug, Clone)]
pub struct TestConversationOptions {
    pub max_sessions: usize,
    pub session_ttl: Duration,
    pub max_messages: usize,
    pub max_bytes: usize,
}

impl Default for TestConversationOptions {
    fn default() -> Self {
        Self {
            max_sessions: 1024,
            session_ttl: Duration::from_secs(3600),
            max_messages: 2048,
            max_bytes: 16 * 1024 * 1024,
        }
    }
}

struct StoredSession {
    generation: String,
    messages: Vec<Message>,
    last_access: Instant,
    active: bool,
}

#[derive(Clone)]
pub struct TestConversationStore {
    sessions: Arc<Mutex<HashMap<ConversationKey, Arc<Mutex<StoredSession>>>>>,
    options: TestConversationOptions,
}

impl Default for TestConversationStore {
    fn default() -> Self {
        Self::new(TestConversationOptions::default()).expect("valid default memory options")
    }
}

impl TestConversationStore {
    pub fn new(options: TestConversationOptions) -> Result<Self, ConversationError> {
        if options.max_sessions == 0
            || options.max_messages == 0
            || options.max_bytes == 0
            || options.session_ttl.is_zero()
        {
            return Err(ConversationError::Other(
                "memory limits and TTL must be positive".into(),
            ));
        }
        Ok(Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            options,
        })
    }

    pub fn snapshot(&self, session_id: &str) -> Vec<Message> {
        let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        sessions
            .get(&ConversationKey::new(session_id))
            .map(|session| {
                session
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .messages
                    .clone()
            })
            .unwrap_or_default()
    }
}

#[async_trait]
impl ConversationStore for TestConversationStore {
    async fn open(
        &self,
        session_id: &str,
    ) -> Result<Box<dyn ConversationSession>, ConversationError> {
        self.open_branch(&ConversationKey::new(session_id)).await
    }
    async fn open_branch(
        &self,
        key: &ConversationKey,
    ) -> Result<Box<dyn ConversationSession>, ConversationError> {
        key.validate()?;
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        sessions.retain(|_, session| {
            let stored = session.lock().unwrap_or_else(|e| e.into_inner());
            stored.active || now.duration_since(stored.last_access) < self.options.session_ttl
        });
        if !sessions.contains_key(key) && sessions.len() >= self.options.max_sessions {
            let oldest = sessions
                .iter()
                .filter_map(|(id, session)| {
                    let stored = session.lock().unwrap_or_else(|e| e.into_inner());
                    (!stored.active).then_some((id.clone(), stored.last_access))
                })
                .min_by_key(|(_, access)| *access)
                .map(|(id, _)| id);
            if let Some(oldest) = oldest {
                sessions.remove(&oldest);
            } else {
                return Err(ConversationError::Other(
                    "all memory sessions are in use".into(),
                ));
            }
        }
        let session = sessions
            .entry(key.clone())
            .or_insert_with(|| {
                Arc::new(Mutex::new(StoredSession {
                    generation: uuid::Uuid::new_v4().to_string(),
                    messages: Vec::new(),
                    last_access: now,
                    active: false,
                }))
            })
            .clone();
        {
            let mut stored = session.lock().unwrap_or_else(|e| e.into_inner());
            if stored.active {
                return Err(ConversationError::Other(
                    "memory session is already in use".into(),
                ));
            }
            stored.active = true;
            stored.last_access = now;
        }
        Ok(Box::new(InMemorySession {
            key: key.clone(),
            session,
            max_messages: self.options.max_messages,
            max_bytes: self.options.max_bytes,
        }))
    }
}

struct InMemorySession {
    key: ConversationKey,
    session: Arc<Mutex<StoredSession>>,
    max_messages: usize,
    max_bytes: usize,
}

impl Drop for InMemorySession {
    fn drop(&mut self) {
        let mut stored = self.session.lock().unwrap_or_else(|e| e.into_inner());
        stored.active = false;
        stored.last_access = Instant::now();
    }
}

#[async_trait]
impl ConversationSession for InMemorySession {
    async fn snapshot(&mut self) -> Result<ConversationSnapshot, ConversationError> {
        let mut stored = self.session.lock().unwrap_or_else(|e| e.into_inner());
        stored.last_access = Instant::now();
        Ok(ConversationSnapshot::new(
            self.key.clone(),
            stored.generation.clone(),
            stored.messages.clone(),
        ))
    }

    async fn append(&mut self, messages: &[Message]) -> Result<(), ConversationError> {
        let mut stored = self.session.lock().unwrap_or_else(|e| e.into_inner());
        let mut next = stored.messages.clone();
        next.extend_from_slice(messages);
        validate_messages(&next)?;
        if next.len() > self.max_messages
            || serde_json::to_vec(&next).map_or(usize::MAX, |v| v.len()) > self.max_bytes
        {
            return Err(ConversationError::BudgetExceeded("in-memory transcript capacity reached; use persistent memory or increase retention capacity".into()));
        }
        stored.messages = next;
        stored.last_access = Instant::now();
        Ok(())
    }
}
