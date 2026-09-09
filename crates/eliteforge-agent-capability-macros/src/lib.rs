//! Attribute macros for declarative capability registration.
//!
//! `#[capability]` deliberately emits only a static descriptor. It does not
//! create an implementation while the crate is loaded; the runtime calls the
//! generated factory for each Agent build instead.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{
    Error, Expr, ExprArray, ItemImpl, Lit, LitStr, Path, Result, Type, parse::Parser, parse2,
};

/// Register a trait implementation as an EliteForge Agent capability.
///
/// The attribute belongs outside `#[async_trait]`:
///
/// ```ignore
/// #[capability(pack = "shop")]
/// #[async_trait]
/// impl ContextProvider for ShopContext { /* ... */ }
/// ```
#[proc_macro_attribute]
pub fn capability(attribute: TokenStream, item: TokenStream) -> TokenStream {
    match expand(attribute.into(), item.into()) {
        Ok(output) => output.into(),
        Err(error) => error.into_compile_error().into(),
    }
}

#[derive(Default)]
struct CapabilityArguments {
    pack: Option<LitStr>,
    id: Option<LitStr>,
    instance: Option<LitStr>,
    depends_on: Option<Vec<LitStr>>,
    config: Option<Type>,
    factory: Option<Path>,
}

impl CapabilityArguments {
    fn parse(input: TokenStream2) -> Result<Self> {
        if input.is_empty() {
            return Err(Error::new(
                proc_macro2::Span::call_site(),
                "missing required `pack = \"...\"` argument",
            ));
        }
        let mut arguments = Self::default();
        let parser = syn::meta::parser(|meta| {
            if meta.path.is_ident("pack") {
                arguments.pack = Some(parse_once_lit_str(meta, "pack", &arguments.pack)?);
            } else if meta.path.is_ident("id") {
                arguments.id = Some(parse_once_lit_str(meta, "id", &arguments.id)?);
            } else if meta.path.is_ident("instance") {
                arguments.instance =
                    Some(parse_once_lit_str(meta, "instance", &arguments.instance)?);
            } else if meta.path.is_ident("depends_on") {
                if arguments.depends_on.is_some() {
                    return Err(meta.error("duplicate `depends_on` argument"));
                }
                let value = meta.value()?;
                let dependencies = value.parse::<ExprArray>().map_err(|_| {
                    meta.error("`depends_on` must be an array of string literals, for example [\"storage\"]")
                })?;
                arguments.depends_on = Some(parse_dependencies(dependencies)?);
            } else if meta.path.is_ident("config") {
                if arguments.config.is_some() {
                    return Err(meta.error("duplicate `config` argument"));
                }
                arguments.config = Some(meta.value()?.parse()?);
            } else if meta.path.is_ident("factory") {
                if arguments.factory.is_some() {
                    return Err(meta.error("duplicate `factory` argument"));
                }
                let factory: Path = meta.value()?.parse()?;
                if factory.segments.len() < 2 {
                    return Err(meta.error(
                        "`factory` must be an associated function path such as Type::from_config",
                    ));
                }
                arguments.factory = Some(factory);
            } else {
                return Err(meta.error(
                    "unsupported capability argument; expected `pack`, `id`, `instance`, `depends_on`, `config`, or `factory`",
                ));
            }
            Ok(())
        });

        parser.parse2(input)?;

        let pack = arguments.pack.as_ref().ok_or_else(|| {
            Error::new(
                proc_macro2::Span::call_site(),
                "missing required `pack = \"...\"` argument",
            )
        })?;
        ensure_non_empty(pack, "pack")?;
        if let Some(id) = &arguments.id {
            ensure_non_empty(id, "id")?;
        }
        if let Some(instance) = &arguments.instance {
            ensure_non_empty(instance, "instance")?;
        }

        match (&arguments.config, &arguments.factory) {
            (Some(_), Some(_)) | (None, None) => Ok(arguments),
            (Some(_), None) => Err(Error::new(
                proc_macro2::Span::call_site(),
                "`config` and `factory` must be specified together",
            )),
            (None, Some(_)) => Err(Error::new(
                proc_macro2::Span::call_site(),
                "`config` and `factory` must be specified together",
            )),
        }
    }
}

fn parse_once_lit_str(
    meta: syn::meta::ParseNestedMeta<'_>,
    name: &str,
    existing: &Option<LitStr>,
) -> Result<LitStr> {
    if existing.is_some() {
        return Err(meta.error(format!("duplicate `{name}` argument")));
    }
    meta.value()?.parse()
}

fn ensure_non_empty(value: &LitStr, name: &str) -> Result<()> {
    if value.value().trim().is_empty() {
        Err(Error::new(
            value.span(),
            format!("`{name}` must not be empty"),
        ))
    } else {
        Ok(())
    }
}

fn parse_dependencies(array: ExprArray) -> Result<Vec<LitStr>> {
    let mut dependencies = Vec::with_capacity(array.elems.len());
    for expression in array.elems {
        let Expr::Lit(literal) = expression else {
            return Err(Error::new_spanned(
                expression,
                "`depends_on` entries must be string literals",
            ));
        };
        let Lit::Str(value) = literal.lit else {
            return Err(Error::new_spanned(
                literal,
                "`depends_on` entries must be string literals",
            ));
        };
        ensure_non_empty(&value, "depends_on entry")?;
        dependencies.push(value);
    }
    Ok(dependencies)
}

#[derive(Clone, Copy)]
enum CapabilityInterface {
    ContextProvider,
    ToolProvider,
    EventSink,
    ContextContributor,
    ModelMiddleware,
    ToolMiddleware,
    RunMiddleware,
    ToolPolicy,
    RunLifecycleHook,
    WorkflowNode,
    WorkflowNodeMiddleware,
    RunOutputFactory,
    Plugin,
}

impl CapabilityInterface {
    fn parse(path: &Path) -> Result<Self> {
        let Some(last_segment) = path.segments.last() else {
            return Err(Error::new_spanned(
                path,
                "capability requires a supported trait implementation",
            ));
        };
        if !matches!(last_segment.arguments, syn::PathArguments::None) {
            return Err(Error::new_spanned(
                last_segment,
                "supported capability traits do not accept generic arguments",
            ));
        }

        match last_segment.ident.to_string().as_str() {
            "ContextProvider" => Ok(Self::ContextProvider),
            "ToolProvider" => Ok(Self::ToolProvider),
            "EventSink" => Ok(Self::EventSink),
            "ContextContributor" => Ok(Self::ContextContributor),
            "ModelMiddleware" => Ok(Self::ModelMiddleware),
            "ToolMiddleware" => Ok(Self::ToolMiddleware),
            "RunMiddleware" => Ok(Self::RunMiddleware),
            "ToolPolicy" => Ok(Self::ToolPolicy),
            "RunLifecycleHook" => Ok(Self::RunLifecycleHook),
            "WorkflowNode" => Ok(Self::WorkflowNode),
            "WorkflowNodeMiddleware" => Ok(Self::WorkflowNodeMiddleware),
            "RunOutputFactory" => Ok(Self::RunOutputFactory),
            "Plugin" => Ok(Self::Plugin),
            _ => Err(Error::new_spanned(
                path,
                "unsupported capability trait; supported traits are ContextProvider, ToolProvider, EventSink, ContextContributor, ModelMiddleware, ToolMiddleware, RunMiddleware, ToolPolicy, RunLifecycleHook, WorkflowNode, WorkflowNodeMiddleware, RunOutputFactory, and Plugin",
            )),
        }
    }

    fn adapter(self) -> Option<TokenStream2> {
        let adapter = match self {
            Self::ContextProvider => quote!(::eliteforge_agent_core::ContextPlugin),
            Self::ToolProvider => quote!(::eliteforge_agent_core::ToolProviderPlugin),
            Self::EventSink => quote!(::eliteforge_agent_core::EventPlugin),
            Self::ContextContributor => quote!(::eliteforge_agent_core::ContextContributorPlugin),
            Self::ModelMiddleware => quote!(::eliteforge_agent_core::ModelMiddlewarePlugin),
            Self::ToolMiddleware => quote!(::eliteforge_agent_core::ToolMiddlewarePlugin),
            Self::RunMiddleware => quote!(::eliteforge_agent_core::RunMiddlewarePlugin),
            Self::ToolPolicy => quote!(::eliteforge_agent_core::ToolPolicyPlugin),
            Self::RunLifecycleHook => quote!(::eliteforge_agent_core::RunLifecycleHookPlugin),
            Self::WorkflowNode => quote!(::eliteforge_agent_core::WorkflowNodePlugin),
            Self::WorkflowNodeMiddleware => {
                quote!(::eliteforge_agent_core::WorkflowNodeMiddlewarePlugin)
            }
            Self::RunOutputFactory => quote!(::eliteforge_agent_core::RunOutputPlugin),
            Self::Plugin => return None,
        };
        Some(adapter)
    }
}

fn expand(attribute: TokenStream2, item: TokenStream2) -> Result<TokenStream2> {
    let arguments = CapabilityArguments::parse(attribute)?;
    let implementation: ItemImpl = parse2(item)?;
    let Some((negative, trait_path, _)) = &implementation.trait_ else {
        return Err(Error::new_spanned(
            &implementation.self_ty,
            "`#[capability]` can only be applied to a supported trait implementation",
        ));
    };
    if negative.is_some() {
        return Err(Error::new_spanned(
            negative,
            "negative trait implementations cannot be registered as capabilities",
        ));
    }
    if !implementation.generics.params.is_empty() {
        return Err(Error::new_spanned(
            &implementation.generics,
            "generic trait implementations cannot be registered as capabilities; use a concrete implementation type",
        ));
    }

    let interface = CapabilityInterface::parse(trait_path)?;
    let self_ty = &implementation.self_ty;
    let where_clause = &implementation.generics.where_clause;
    let pack = arguments
        .pack
        .expect("validated by CapabilityArguments::parse");
    let dependencies = arguments.depends_on.unwrap_or_default();

    let trait_name = trait_path
        .segments
        .last()
        .expect("trait path is non-empty")
        .ident
        .to_string();
    let default_id = quote!(concat!(
        #pack,
        "::",
        module_path!(),
        "::",
        stringify!(#self_ty),
        "::",
        #trait_name
    ));
    let id = arguments.id.map_or(default_id, |value| quote!(#value));
    // `instance` intentionally omits the trait: one concrete implementation can
    // expose several capability traits while the runtime creates it only once.
    let default_instance = default_instance_key(&pack, self_ty);
    let is_default_instance = arguments.instance.is_none();
    let instance = arguments
        .instance
        .map_or(default_instance, |value| quote!(#value));

    let (factory_signature, factory_body, type_identity) = match (
        arguments.config,
        arguments.factory,
    ) {
        (Some(config), Some(factory)) => {
            let signature = quote!(concat!(
                #pack,
                "::",
                module_path!(),
                "::",
                stringify!(#self_ty),
                "::config=",
                stringify!(#config),
                "::factory=",
                stringify!(#factory)
            ));
            let body = quote! {
                let configuration = configuration.downcast_ref::<#config>().ok_or_else(|| {
                    ::eliteforge_agent_core::CapabilityError::configuration_type(
                        #id,
                        stringify!(#config),
                    )
                })?;
                let implementation: #self_ty = #factory(configuration);
                Ok(::std::sync::Arc::new(implementation))
            };
            let identity =
                quote!(.with_type_identity::<#self_ty, #config, _>(#factory, #is_default_instance));
            (signature, body, identity)
        }
        (None, None) => {
            let signature = default_factory_signature(&pack, self_ty);
            let body = quote! {
                let _ = configuration;
                let implementation: #self_ty =
                    <#self_ty as ::core::default::Default>::default();
                Ok(::std::sync::Arc::new(implementation))
            };
            let identity = quote!(.with_type_identity::<#self_ty, (), _>(
                <#self_ty as ::core::default::Default>::default,
                #is_default_instance,
            ));
            (signature, body, identity)
        }
        _ => unreachable!("arguments are validated before expansion"),
    };

    let adapter_body = match interface.adapter() {
        Some(adapter) => quote! {
            let implementation = ::std::sync::Arc::downcast::<#self_ty>(instance).map_err(|_| {
                ::eliteforge_agent_core::CapabilityError::instance_type(
                    #id,
                    stringify!(#self_ty),
                )
            })?;
            let implementation: ::std::sync::Arc<dyn #trait_path> = implementation;
            Ok(::std::sync::Arc::new(#adapter::from_arc(#id, implementation)))
        },
        None => quote! {
            let implementation = ::std::sync::Arc::downcast::<#self_ty>(instance).map_err(|_| {
                ::eliteforge_agent_core::CapabilityError::instance_type(
                    #id,
                    stringify!(#self_ty),
                )
            })?;
            let implementation: ::std::sync::Arc<dyn #trait_path> = implementation;
            Ok(implementation)
        },
    };

    Ok(quote! {
        #implementation

        // Each declaration owns its helpers, including stacked declarations on
        // one impl with different IDs or instances.
        const _: () = {
            #[doc(hidden)]
            fn factory(
                configuration: &::eliteforge_agent_core::CapabilityConfig,
            ) -> ::core::result::Result<
                ::eliteforge_agent_core::CapabilityInstance,
                ::eliteforge_agent_core::CapabilityError,
            > #where_clause {
                #factory_body
            }

            #[doc(hidden)]
            fn adapter(
                instance: ::eliteforge_agent_core::CapabilityInstance,
            ) -> ::core::result::Result<
                ::std::sync::Arc<dyn ::eliteforge_agent_core::Plugin>,
                ::eliteforge_agent_core::CapabilityError,
            > #where_clause {
                #adapter_body
            }

            ::eliteforge_agent_core::__private::inventory::submit! {
                ::eliteforge_agent_core::CapabilityDescriptor::new(
                    #pack,
                    #id,
                    #instance,
                    #factory_signature,
                    &[#(#dependencies),*],
                    factory,
                    adapter,
                ) #type_identity
            }
        };
    })
}

fn default_instance_key(pack: &LitStr, self_ty: &Type) -> TokenStream2 {
    quote!(concat!(
        #pack,
        "::",
        module_path!(),
        "::",
        stringify!(#self_ty)
    ))
}

fn default_factory_signature(pack: &LitStr, self_ty: &Type) -> TokenStream2 {
    quote!(concat!(
        #pack,
        "::",
        module_path!(),
        "::",
        stringify!(#self_ty),
        "::default"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use quote::quote;

    fn context_provider_impl() -> TokenStream2 {
        quote! {
            #[async_trait]
            impl ContextProvider for ShopContext {
                async fn context(&self, _: &RunContext) -> Result<Vec<Message>, AgentError> {
                    Ok(Vec::new())
                }
            }
        }
    }

    #[test]
    fn expands_async_trait_impl_and_static_registration() {
        let output = expand(
            quote!(pack = "shop", depends_on = ["storage", "auth"]),
            context_provider_impl(),
        )
        .expect("valid capability expands");
        let output = output.to_string();

        assert!(output.contains("async_trait"));
        assert!(output.contains("inventory :: submit"));
        assert!(output.contains("ContextPlugin :: from_arc"));
        assert!(output.contains("CapabilityDescriptor :: new"));
        assert!(output.contains("module_path"));
    }

    #[test]
    fn requires_pack() {
        let error = expand(quote!(), context_provider_impl()).expect_err("pack is required");
        assert!(
            error.to_string().contains("missing required `pack ="),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn requires_config_and_factory_together() {
        let config_only = expand(
            quote!(pack = "shop", config = ShopConfig),
            context_provider_impl(),
        )
        .expect_err("config alone is invalid");
        assert!(
            config_only
                .to_string()
                .contains("`config` and `factory` must be specified together")
        );

        let factory_only = expand(
            quote!(pack = "shop", factory = ShopContext::from_config),
            context_provider_impl(),
        )
        .expect_err("factory alone is invalid");
        assert!(
            factory_only
                .to_string()
                .contains("`config` and `factory` must be specified together")
        );
    }

    #[test]
    fn rejects_unknown_trait() {
        let error = expand(
            quote!(pack = "shop"),
            quote!(impl Display for ShopContext {}),
        )
        .expect_err("unsupported traits must be rejected");
        assert!(error.to_string().contains("unsupported capability trait"));
    }

    #[test]
    fn rejects_non_string_dependency() {
        let error = expand(
            quote!(pack = "shop", depends_on = ["storage", dependency_id]),
            context_provider_impl(),
        )
        .expect_err("dependency names must be literals");
        assert!(
            error
                .to_string()
                .contains("`depends_on` entries must be string literals")
        );
    }

    #[test]
    fn expands_configured_factory() {
        let output = expand(
            quote!(
                pack = "shop",
                config = ShopConfig,
                factory = ShopContext::from_config
            ),
            context_provider_impl(),
        )
        .expect("configured capability expands")
        .to_string();

        assert!(output.contains("downcast_ref"));
        assert!(output.contains("ShopConfig"));
        assert!(output.contains("ShopContext :: from_config"));
    }

    #[test]
    fn maps_every_supported_capability_trait() {
        let cases = [
            ("ContextProvider", Some("ContextPlugin")),
            ("ToolProvider", Some("ToolProviderPlugin")),
            ("EventSink", Some("EventPlugin")),
            ("ContextContributor", Some("ContextContributorPlugin")),
            ("ModelMiddleware", Some("ModelMiddlewarePlugin")),
            ("ToolMiddleware", Some("ToolMiddlewarePlugin")),
            ("RunMiddleware", Some("RunMiddlewarePlugin")),
            ("ToolPolicy", Some("ToolPolicyPlugin")),
            ("RunLifecycleHook", Some("RunLifecycleHookPlugin")),
            ("WorkflowNode", Some("WorkflowNodePlugin")),
            (
                "WorkflowNodeMiddleware",
                Some("WorkflowNodeMiddlewarePlugin"),
            ),
            ("RunOutputFactory", Some("RunOutputPlugin")),
            ("Plugin", None),
        ];

        for (trait_name, expected_adapter) in cases {
            let path = syn::parse_str::<Path>(trait_name).expect("test trait path parses");
            let interface = CapabilityInterface::parse(&path).expect("trait is supported");
            match expected_adapter {
                Some(expected_adapter) => assert!(
                    interface
                        .adapter()
                        .expect("ordinary capability has an adapter")
                        .to_string()
                        .contains(expected_adapter),
                    "{trait_name} should map to {expected_adapter}"
                ),
                None => assert!(
                    interface.adapter().is_none(),
                    "{trait_name} is direct Plugin"
                ),
            }
        }
    }

    #[test]
    fn default_shared_identity_does_not_include_the_trait() {
        let pack = syn::parse_str::<LitStr>("\"shop\"").expect("test pack parses");
        let implementation =
            syn::parse_str::<Type>("SharedCapability").expect("test implementation type parses");

        let instance = default_instance_key(&pack, &implementation).to_string();
        let factory_signature = default_factory_signature(&pack, &implementation).to_string();

        assert!(!instance.contains("ContextProvider"));
        assert!(!instance.contains("EventSink"));
        assert!(!factory_signature.contains("ContextProvider"));
        assert!(!factory_signature.contains("EventSink"));
    }
}
