// Generated forwards preserve the public execution API.
impl Agent {
    pub fn models(&self, operation: ModelOperation) -> Vec<ModelRef> {
        let generation = self.snapshot();
        generation.models(operation)
    }
    pub async fn embed(&self, request: EmbeddingRequest) -> Result<EmbeddingResponse, AgentError> {
        let (generation, _lease) = self.admit("operation", "embed")?;
        generation.embed(request).await
    }
    pub async fn embed_query(
        &self,
        space: &EmbeddingSpace,
        request: EmbeddingRequest,
    ) -> Result<EmbeddingResponse, AgentError> {
        let (generation, _lease) = self.admit("operation", "embed_query")?;
        generation.embed_query(space, request).await
    }
    pub async fn rerank(&self, request: RerankRequest) -> Result<RerankResponse, AgentError> {
        let (generation, _lease) = self.admit("operation", "rerank")?;
        generation.rerank(request).await
    }
    pub async fn generate_image(
        &self,
        request: ImageGenerationRequest,
    ) -> Result<ImageGenerationResponse, AgentError> {
        let (generation, _lease) = self.admit("operation", "generate_image")?;
        generation.generate_image(request).await
    }
    pub async fn submit_image(
        &self,
        request: ImageGenerationRequest,
    ) -> Result<ImageJob, AgentError> {
        let (generation, _lease) = self.admit("operation", "submit_image")?;
        generation.submit_image(request).await
    }
    pub async fn query_image(&self, job: &ImageJob) -> Result<ImageJob, AgentError> {
        let (generation, _lease) = self.admit("operation", "query_image")?;
        generation.query_image(job).await
    }
    pub async fn cancel_image(&self, job: &ImageJob) -> Result<(), AgentError> {
        let (generation, _lease) = self.admit("operation", "cancel_image")?;
        generation.cancel_image(job).await
    }
    pub async fn detect_content(
        &self,
        request: ContentDetectionRequest,
    ) -> Result<ContentDetectionResponse, AgentError> {
        let (generation, _lease) = self.admit("operation", "detect_content")?;
        generation.detect_content(request).await
    }
    pub async fn submit_video(
        &self,
        request: VideoGenerationRequest,
    ) -> Result<VideoJob, AgentError> {
        let (generation, _lease) = self.admit("operation", "submit_video")?;
        generation.submit_video(request).await
    }
    pub async fn query_video(&self, job: &VideoJob) -> Result<VideoJob, AgentError> {
        let (generation, _lease) = self.admit("operation", "query_video")?;
        generation.query_video(job).await
    }
    pub async fn cancel_video(&self, job: &VideoJob) -> Result<(), AgentError> {
        let (generation, _lease) = self.admit("operation", "cancel_video")?;
        generation.cancel_video(job).await
    }
    pub async fn transcribe(
        &self,
        request: SpeechRecognitionRequest,
    ) -> Result<SpeechRecognitionResponse, AgentError> {
        let (generation, _lease) = self.admit("operation", "transcribe")?;
        generation.transcribe(request).await
    }
    pub async fn synthesize(
        &self,
        request: SpeechSynthesisRequest,
    ) -> Result<SpeechSynthesisResponse, AgentError> {
        let (generation, _lease) = self.admit("operation", "synthesize")?;
        generation.synthesize(request).await
    }
    pub async fn send_realtime(
        &self,
        handle: &RealtimeHandle,
        input: RealtimeInput,
    ) -> Result<(), AgentError> {
        let (generation, _lease) = self.admit("operation", "send_realtime")?;
        generation.send_realtime(handle, input).await
    }
    pub async fn next_realtime(
        &self,
        handle: &RealtimeHandle,
    ) -> Result<Option<RealtimeEvent>, AgentError> {
        let (generation, _lease) = self.admit("operation", "next_realtime")?;
        generation.next_realtime(handle).await
    }
    pub async fn execute<O: AgentOperation>(&self, operation: O) -> Result<O::Output, AgentError> {
        let (generation, _lease) = self.admit("operation", "execute")?;
        generation.execute::<O>(operation).await
    }
    pub async fn pending_interactions(
        &self,
        session: &str,
    ) -> Result<Vec<InteractionRecord>, AgentError> {
        let (generation, _lease) = self.admit("operation", "pending_interactions")?;
        generation.pending_interactions(session).await
    }
    pub async fn interaction(
        &self,
        session: &str,
        interaction_id: RunId,
    ) -> Result<InteractionRecord, AgentError> {
        let (generation, _lease) = self.admit("operation", "interaction")?;
        generation.interaction(session, interaction_id).await
    }
    pub async fn checkpoint(&self, run_id: RunId) -> Result<Checkpoint, AgentError> {
        let (generation, _lease) = self.admit("operation", "checkpoint")?;
        generation.checkpoint(run_id).await
    }
    pub async fn interactions(
        &self,
        session: &str,
        interaction_ids: &[RunId],
    ) -> Result<Vec<InteractionRecord>, AgentError> {
        let (generation, _lease) = self.admit("operation", "interactions")?;
        generation.interactions(session, interaction_ids).await
    }
    pub async fn latest_checkpoint(&self, session: &str) -> Result<Option<Checkpoint>, AgentError> {
        let (generation, _lease) = self.admit("operation", "latest_checkpoint")?;
        generation.latest_checkpoint(session).await
    }
    pub async fn answer(
        &self,
        session: &str,
        interaction_id: RunId,
        value: serde_json::Value,
    ) -> Result<RunRequest, AgentError> {
        let (generation, _lease) = self.admit("operation", "answer")?;
        generation.answer(session, interaction_id, value).await
    }
    pub async fn answer_with_provenance(
        &self,
        session: &str,
        interaction_id: RunId,
        value: serde_json::Value,
        provenance: Option<MessageProvenance>,
    ) -> Result<RunRequest, AgentError> {
        let (generation, _lease) = self.admit("operation", "answer_with_provenance")?;
        generation
            .answer_with_provenance(session, interaction_id, value, provenance)
            .await
    }
    pub async fn conversation(
        &self,
        key: &ConversationKey,
    ) -> Result<ConversationSnapshot, AgentError> {
        let (generation, _lease) = self.admit("operation", "conversation")?;
        generation.conversation(key).await
    }
    pub async fn append_conversation(
        &self,
        key: &ConversationKey,
        revision: u64,
        messages: Vec<Message>,
    ) -> Result<(), AgentError> {
        let (generation, _lease) = self.admit("operation", "append_conversation")?;
        generation
            .append_conversation(key, revision, messages)
            .await
    }
    pub async fn memory(&self, namespace: &str, id: &str) -> Result<MemoryEntry, AgentError> {
        let (generation, _lease) = self.admit("operation", "memory")?;
        generation.memory(namespace, id).await
    }
    pub async fn search_memory(
        &self,
        namespace: &str,
        query: MemoryQuery,
    ) -> Result<Vec<MemoryEntry>, AgentError> {
        let (generation, _lease) = self.admit("operation", "search_memory")?;
        generation.search_memory(namespace, query).await
    }
    pub async fn save_memory(
        &self,
        namespace: &str,
        value: MemoryWrite,
        expected: Option<String>,
    ) -> Result<MemoryEntry, AgentError> {
        let (generation, _lease) = self.admit("operation", "save_memory")?;
        generation.save_memory(namespace, value, expected).await
    }
    pub async fn delete_memory(
        &self,
        namespace: &str,
        id: &str,
        expected: &str,
    ) -> Result<(), AgentError> {
        let (generation, _lease) = self.admit("operation", "delete_memory")?;
        generation.delete_memory(namespace, id, expected).await
    }
    pub fn output<T: Send + 'static>(&self, handle: &mut RunHandle) -> Result<T, AgentError> {
        let generation = self.snapshot();
        generation.output::<T>(handle)
    }
    pub fn control(&self, handle: &RunHandle) -> Result<RunControl, AgentError> {
        let generation = self.snapshot();
        generation.control(handle)
    }
    pub fn cancel(&self, run: &impl AsRef<RunControl>) -> Result<(), AgentError> {
        let generation = self.snapshot();
        generation.cancel(run)
    }
    pub fn steer(
        &self,
        run: &impl AsRef<RunControl>,
        message: impl Into<String>,
    ) -> Result<(), AgentError> {
        let generation = self.snapshot();
        generation.steer(run, message)
    }
    pub fn steer_content(
        &self,
        run: &impl AsRef<RunControl>,
        content: Content,
    ) -> Result<(), AgentError> {
        self.snapshot().steer_content(run, content)
    }
    pub fn revoke_tools(
        &self,
        run: &impl AsRef<RunControl>,
        ids: &[String],
    ) -> Result<(), AgentError> {
        let generation = self.snapshot();
        generation.revoke_tools(run, ids)
    }
    pub fn budget_usage(&self, run: &impl AsRef<RunControl>) -> Result<Usage, AgentError> {
        let generation = self.snapshot();
        generation.budget_usage(run)
    }
    pub fn execution_snapshot(
        &self,
        run: &impl AsRef<RunControl>,
        result: &RunResult,
    ) -> Result<RunExecutionSnapshot, AgentError> {
        self.snapshot().execution_snapshot(run, result)
    }
    pub async fn start_child_owned(
        &self,
        request: RunRequest,
        parent: &RunContext,
    ) -> Result<ChildRunHandle, AgentError> {
        let (generation, lease) = self.admit("task", &request.session_id)?;
        let mut child = generation.start_child_owned(request, parent).await?;
        child.handle.as_mut().expect("child handle").occupancy = Some(lease);
        Ok(child)
    }
    pub async fn start_child_with_output_owned<T: Send + 'static>(
        &self,
        request: RunRequest,
        parent: &RunContext,
    ) -> Result<(ChildRunHandle, T), AgentError> {
        let (generation, lease) = self.admit("task", &request.session_id)?;
        let (mut child, output) = generation
            .start_child_with_output_owned(request, parent)
            .await?;
        child.handle.as_mut().expect("child handle").occupancy = Some(lease);
        Ok((child, output))
    }
    /// Detached variant of [`start_child_owned`]: the child outlives the parent
    /// run and is governed by the caller that owns the returned handle.
    pub async fn start_child_detached_owned(
        &self,
        request: RunRequest,
        parent: &RunContext,
    ) -> Result<ChildRunHandle, AgentError> {
        let (generation, lease) = self.admit("task", &request.session_id)?;
        let mut child = generation
            .start_child_detached_owned(request, parent)
            .await?;
        child.handle.as_mut().expect("child handle").occupancy = Some(lease);
        Ok(child)
    }
    pub async fn start_child_detached_with_output_owned<T: Send + 'static>(
        &self,
        request: RunRequest,
        parent: &RunContext,
    ) -> Result<(ChildRunHandle, T), AgentError> {
        let (generation, lease) = self.admit("task", &request.session_id)?;
        let (mut child, output) = generation
            .start_child_detached_with_output_owned(request, parent)
            .await?;
        child.handle.as_mut().expect("child handle").occupancy = Some(lease);
        Ok((child, output))
    }
    pub fn events(
        &self,
        handle: &mut RunHandle,
    ) -> Result<broadcast::Receiver<EventEnvelope>, AgentError> {
        let generation = self.snapshot();
        generation.events(handle)
    }
    pub fn subscribe(
        &self,
        handle: &RunHandle,
    ) -> Result<broadcast::Receiver<EventEnvelope>, AgentError> {
        let generation = self.snapshot();
        generation.subscribe(handle)
    }
    pub async fn wait(&self, handle: RunHandle) -> Result<RunResult, AgentError> {
        let generation = self.snapshot();
        generation.wait(handle).await
    }
    pub async fn run(&self, request: RunRequest) -> Result<RunResult, AgentError> {
        let (generation, _lease) = self.admit("operation", "run")?;
        generation.run(request).await
    }
    pub async fn run_child(
        &self,
        request: RunRequest,
        parent: &RunContext,
    ) -> Result<RunResult, AgentError> {
        if self.is_closed() {
            return self.snapshot().run_child(request, parent).await;
        }
        let (generation, _lease) = self.admit("operation", "run_child")?;
        generation.run_child(request, parent).await
    }
}
