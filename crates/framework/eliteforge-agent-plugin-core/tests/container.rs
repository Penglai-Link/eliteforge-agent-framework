use std::{
    future::pending,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use eliteforge_agent_plugin_core::{
    Plugin, PluginContainer, PluginContext, PluginError, PluginReadyContext, async_trait,
};
use tokio::sync::Notify;

type Installer = dyn Fn(&mut PluginContext<'_>) -> Result<(), PluginError> + Send + Sync;
type Log = Arc<Mutex<Vec<String>>>;

struct TestPlugin {
    id: String,
    dependencies: Vec<String>,
    installer: Box<Installer>,
}

#[async_trait]
impl Plugin for TestPlugin {
    fn id(&self) -> &str {
        &self.id
    }

    fn dependencies(&self) -> Vec<String> {
        self.dependencies.clone()
    }

    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        (self.installer)(context)
    }
}

fn plugin(
    id: &str,
    dependencies: &[&str],
    install: impl Fn(&mut PluginContext<'_>) -> Result<(), PluginError> + Send + Sync + 'static,
) -> Arc<dyn Plugin> {
    Arc::new(TestPlugin {
        id: id.to_owned(),
        dependencies: dependencies
            .iter()
            .map(|value| (*value).to_owned())
            .collect(),
        installer: Box::new(install),
    })
}

fn cleanup_plugin(id: &str, log: &Log) -> Arc<dyn Plugin> {
    let log = log.clone();
    let id = id.to_owned();
    plugin(&id.clone(), &[], move |context| {
        for suffix in ["first", "second"] {
            let log = log.clone();
            let value = format!("{id}.{suffix}");
            context.on_cleanup(move || log.lock().unwrap().push(value));
        }
        Ok(())
    })
}

fn log_values(log: &Log) -> Vec<String> {
    log.lock().unwrap().clone()
}

fn error_of(result: Result<PluginContainer, PluginError>) -> PluginError {
    match result {
        Ok(_) => panic!("expected failed build"),
        Err(error) => error,
    }
}

#[tokio::test]
async fn empty_container_and_missing_services_are_supported() {
    let mut container = PluginContainer::build(Vec::new()).await.unwrap();
    assert!(container.plugin_ids().is_empty());
    assert!(!container.is_closed());
    assert!(!container.services().contains::<String>());
    assert!(container.services().get::<String>().is_none());
    assert!(container.services().all::<String>().is_empty());
    assert!(container.services().providers::<String>().is_empty());
    container.close();
    assert!(container.is_closed());
}

#[tokio::test]
async fn installs_dependencies_before_dependents_with_stable_input_ties() {
    let log: Log = Arc::default();
    let child_log = log.clone();
    let independent_log = log.clone();
    let parent_log = log.clone();
    let child = plugin("child", &["parent", "parent"], move |context| {
        assert_eq!(**context.services().get::<u32>().as_ref().unwrap(), 42);
        child_log.lock().unwrap().push("child".into());
        Ok(())
    });
    let independent = plugin("independent", &[], move |_| {
        independent_log.lock().unwrap().push("independent".into());
        Ok(())
    });
    let parent = plugin("parent", &[], move |context| {
        context.provide(42_u32)?;
        parent_log.lock().unwrap().push("parent".into());
        Ok(())
    });
    let container = PluginContainer::build(vec![child, independent, parent])
        .await
        .unwrap();
    assert_eq!(log_values(&log), ["independent", "parent", "child"]);
    assert_eq!(container.plugin_ids(), ["independent", "parent", "child"]);
    assert_eq!(container.services().providers::<u32>(), ["parent"]);
}

#[tokio::test]
async fn rejects_invalid_graphs_before_any_plugin_is_installed() {
    for case in ["empty", "duplicate", "missing", "cycle", "self_cycle"] {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let first = plugin("valid", &[], move |_| {
            observed.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        let mut plugins = vec![first];
        match case {
            "empty" => plugins.push(plugin("  ", &[], |_| Ok(()))),
            "duplicate" => plugins.push(plugin("valid", &[], |_| Ok(()))),
            "missing" => plugins.push(plugin("dependent", &["absent"], |_| Ok(()))),
            "cycle" => {
                plugins.push(plugin("a", &["b"], |_| Ok(())));
                plugins.push(plugin("b", &["a"], |_| Ok(())));
            }
            "self_cycle" => plugins.push(plugin("a", &["a"], |_| Ok(()))),
            _ => unreachable!(),
        }
        let error = error_of(PluginContainer::build(plugins).await);
        match case {
            "empty" => assert!(matches!(error, PluginError::EmptyPluginId { index: 1 })),
            "duplicate" => assert!(matches!(error, PluginError::DuplicatePluginId { .. })),
            "missing" => assert!(matches!(error, PluginError::MissingDependency { .. })),
            _ => assert!(matches!(error, PluginError::DependencyCycle { .. })),
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0, "invalid case: {case}");
    }
}

#[tokio::test]
async fn unique_and_contribution_services_preserve_types_and_order() {
    let container = PluginContainer::build(vec![
        plugin("one", &[], |context| {
            context.provide(7_u32)?;
            context.contribute("first".to_owned())?;
            context.contribute("second".to_owned())
        }),
        plugin("two", &[], |context| context.contribute("third".to_owned())),
    ])
    .await
    .unwrap();
    assert_eq!(*container.services().get::<u32>().unwrap(), 7);
    assert!(container.services().all::<u32>().is_empty());
    assert!(container.services().get::<String>().is_none());
    let contributions = container.services().all::<String>();
    assert_eq!(
        contributions
            .iter()
            .map(|value| value.as_str())
            .collect::<Vec<_>>(),
        ["first", "second", "third"]
    );
    assert!(container.services().contains::<String>());
    assert_eq!(
        container.services().providers::<String>(),
        ["one", "one", "two"]
    );
}

#[tokio::test]
async fn rejects_duplicate_unique_and_both_mixed_registration_orders() {
    for (first_unique, second_unique) in [(true, true), (true, false), (false, true)] {
        let cleaned = Arc::new(AtomicUsize::new(0));
        let cleanup_observed = cleaned.clone();
        let first = plugin("first", &[], move |context| {
            if first_unique {
                context.provide(1_u32)
            } else {
                context.contribute(1_u32)
            }
        });
        let second = plugin("second", &[], move |context| {
            let cleanup_observed = cleanup_observed.clone();
            context.on_cleanup(move || {
                cleanup_observed.fetch_add(1, Ordering::SeqCst);
            });
            if second_unique {
                context.provide(2_u32)
            } else {
                context.contribute(2_u32)
            }
        });
        let error = error_of(PluginContainer::build(vec![first, second]).await);
        match error {
            PluginError::InstallationFailed { plugin, source } => {
                assert_eq!(plugin, "second");
                if first_unique && second_unique {
                    assert!(matches!(*source, PluginError::DuplicateService { .. }));
                } else {
                    assert!(matches!(*source, PluginError::ServiceKindConflict { .. }));
                }
            }
            _ => panic!("expected attributed registration error"),
        }
        assert_eq!(cleaned.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn close_releases_resources_in_reverse_order_and_is_idempotent() {
    let log: Log = Arc::default();
    let mut container = PluginContainer::build(vec![
        cleanup_plugin("one", &log),
        cleanup_plugin("two", &log),
        plugin("service", &[], |context| context.provide(7_u32)),
    ])
    .await
    .unwrap();
    let external_service = container.services().get::<u32>().unwrap();
    assert_eq!(Arc::strong_count(&external_service), 2);
    container.close();
    assert!(container.is_closed());
    assert!(!container.services().contains::<u32>());
    assert_eq!(Arc::strong_count(&external_service), 1);
    assert_eq!(
        log_values(&log),
        ["two.second", "two.first", "one.second", "one.first"]
    );
    assert_eq!(container.plugin_ids(), ["one", "two", "service"]);
    container.close();
    drop(container);
    assert_eq!(log_values(&log).len(), 4);
}

#[tokio::test]
async fn drop_runs_cleanup_without_explicit_close() {
    let log: Log = Arc::default();
    let container = PluginContainer::build(vec![cleanup_plugin("one", &log)])
        .await
        .unwrap();
    drop(container);
    assert_eq!(log_values(&log), ["one.second", "one.first"]);
}

struct TrackedService(Arc<AtomicUsize>);

impl Drop for TrackedService {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn failed_install_rolls_back_partial_resources_and_prior_plugins() {
    let log: Log = Arc::default();
    let failed_log = log.clone();
    let drops = Arc::new(AtomicUsize::new(0));
    let observed_drops = drops.clone();
    let failed = plugin("failed", &[], move |context| {
        context.provide(TrackedService(observed_drops.clone()))?;
        let failed_log = failed_log.clone();
        context.on_cleanup(move || failed_log.lock().unwrap().push("failed.partial".into()));
        Err(PluginError::message("connection refused"))
    });
    let error = error_of(PluginContainer::build(vec![cleanup_plugin("ready", &log), failed]).await);
    assert_eq!(
        error.to_string(),
        "plugin `failed` failed to install: connection refused"
    );
    assert_eq!(
        log_values(&log),
        ["failed.partial", "ready.second", "ready.first"]
    );
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

struct PendingPlugin {
    started: Arc<Notify>,
    log: Log,
    drops: Arc<AtomicUsize>,
}

#[async_trait]
impl Plugin for PendingPlugin {
    fn id(&self) -> &str {
        "pending"
    }

    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        context.provide(TrackedService(self.drops.clone()))?;
        let log = self.log.clone();
        context.on_cleanup(move || log.lock().unwrap().push("pending.partial".into()));
        self.started.notify_one();
        pending::<()>().await;
        Ok(())
    }
}

#[tokio::test]
async fn cancelling_build_releases_partial_and_previously_installed_resources() {
    let log: Log = Arc::default();
    let started = Arc::new(Notify::new());
    let drops = Arc::new(AtomicUsize::new(0));
    let plugins = vec![
        cleanup_plugin("ready", &log),
        Arc::new(PendingPlugin {
            started: started.clone(),
            log: log.clone(),
            drops: drops.clone(),
        }),
    ];
    let build = tokio::spawn(PluginContainer::build(plugins));
    tokio::time::timeout(std::time::Duration::from_secs(2), started.notified())
        .await
        .unwrap();
    build.abort();
    assert!(matches!(build.await, Err(error) if error.is_cancelled()));
    assert_eq!(
        log_values(&log),
        ["pending.partial", "ready.second", "ready.first"]
    );
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cleanup_panics_do_not_skip_remaining_callbacks_or_repeat_cleanup() {
    let log: Log = Arc::default();
    let captured = log.clone();
    let panicking = plugin("panic", &[], move |context| {
        let captured = captured.clone();
        context.on_cleanup(move || captured.lock().unwrap().push("after panic".into()));
        context.on_cleanup(|| panic!("cleanup failed"));
        Ok(())
    });
    let mut container = PluginContainer::build(vec![panicking]).await.unwrap();
    container.close();
    assert_eq!(container.cleanup_panic_count(), 1);
    assert_eq!(log_values(&log), ["after panic"]);
    container.close();
    assert_eq!(container.cleanup_panic_count(), 1);
}

#[tokio::test]
async fn cleanup_panic_during_unwinding_does_not_double_panic() {
    let log: Log = Arc::default();
    let container = PluginContainer::build(vec![
        cleanup_plugin("survivor", &log),
        plugin("panic", &[], |context| {
            context.on_cleanup(|| panic!("cleanup panic during unwind"));
            Ok(())
        }),
    ])
    .await
    .unwrap();
    let result = catch_unwind(AssertUnwindSafe(move || {
        let _container = container;
        panic!("outer panic");
    }));
    assert!(result.is_err());
    assert_eq!(log_values(&log), ["survivor.second", "survivor.first"]);
}

#[tokio::test]
async fn containers_isolate_service_values_and_resource_lifetimes() {
    let one_log: Log = Arc::default();
    let two_log: Log = Arc::default();
    let mut one = PluginContainer::build(vec![
        plugin("service", &[], |context| context.provide(1_u32)),
        cleanup_plugin("resource", &one_log),
    ])
    .await
    .unwrap();
    let two = PluginContainer::build(vec![
        plugin("service", &[], |context| context.provide(2_u32)),
        cleanup_plugin("resource", &two_log),
    ])
    .await
    .unwrap();
    assert_eq!(*one.services().get::<u32>().unwrap(), 1);
    assert_eq!(*two.services().get::<u32>().unwrap(), 2);
    one.close();
    assert_eq!(one_log.lock().unwrap().len(), 2);
    assert!(two_log.lock().unwrap().is_empty());
    assert_eq!(*two.services().get::<u32>().unwrap(), 2);
    drop(two);
    assert_eq!(two_log.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn supports_trait_object_services_and_shareable_container() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<PluginContainer>();
    trait Greeter: Send + Sync {
        fn greet(&self) -> &str;
    }
    struct Hello;
    impl Greeter for Hello {
        fn greet(&self) -> &str {
            "hello"
        }
    }
    let container = PluginContainer::build(vec![plugin("greeting", &[], |context| {
        context.provide::<Arc<dyn Greeter>>(Arc::new(Hello))
    })])
    .await
    .unwrap();
    assert_eq!(
        container
            .services()
            .get::<Arc<dyn Greeter>>()
            .unwrap()
            .greet(),
        "hello"
    );
}

struct ReadyConsumer {
    observed: Arc<AtomicUsize>,
    cleaned: Arc<AtomicUsize>,
    fail: bool,
}

#[async_trait]
impl Plugin for ReadyConsumer {
    fn id(&self) -> &str {
        "ready-consumer"
    }

    async fn install(&self, _: &mut PluginContext<'_>) -> Result<(), PluginError> {
        Ok(())
    }

    async fn ready(&self, context: &mut PluginReadyContext<'_>) -> Result<(), PluginError> {
        let value = context.services().get::<usize>().ok_or_else(|| {
            PluginError::message("ready phase did not see a later installed service")
        })?;
        self.observed.store(*value, Ordering::SeqCst);
        let cleaned = self.cleaned.clone();
        context.on_cleanup(move || {
            cleaned.fetch_add(1, Ordering::SeqCst);
        });
        if self.fail {
            Err(PluginError::message("ready failed"))
        } else {
            Ok(())
        }
    }
}

#[tokio::test]
async fn ready_observes_the_complete_registry_and_failure_rolls_back() {
    let observed = Arc::new(AtomicUsize::new(0));
    let cleaned = Arc::new(AtomicUsize::new(0));
    let consumer = Arc::new(ReadyConsumer {
        observed: observed.clone(),
        cleaned: cleaned.clone(),
        fail: false,
    });
    let container = PluginContainer::build(vec![
        consumer,
        plugin("late-producer", &[], |context| context.provide(42_usize)),
    ])
    .await
    .unwrap();
    assert_eq!(observed.load(Ordering::SeqCst), 42);
    drop(container);
    assert_eq!(cleaned.load(Ordering::SeqCst), 1);

    let failed_cleaned = Arc::new(AtomicUsize::new(0));
    let error = error_of(
        PluginContainer::build(vec![
            Arc::new(ReadyConsumer {
                observed: Arc::new(AtomicUsize::new(0)),
                cleaned: failed_cleaned.clone(),
                fail: true,
            }),
            plugin("failed-ready-producer", &[], |context| {
                context.provide(7_usize)
            }),
        ])
        .await,
    );
    assert!(matches!(error, PluginError::ReadyFailed { plugin, .. } if plugin == "ready-consumer"));
    assert_eq!(failed_cleaned.load(Ordering::SeqCst), 1);
}
