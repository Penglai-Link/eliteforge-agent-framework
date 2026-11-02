//! Attribute macros for declarative capability registration.
//!
//! `#[capability]` deliberately emits only a static descriptor. It does not
//! create an implementation while the crate is loaded; the runtime calls the
//! generated factory for each Agent build instead.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path as FsPath, PathBuf},
};
use syn::{
    Error, Expr, ExprArray, FnArg, ItemFn, ItemImpl, ItemTrait, Lit, LitStr, Path, Result,
    ReturnType, Type, parse::Parser, parse2,
};

/// Register a trait implementation as an EliteForge Agent capability.
///
/// The attribute belongs outside `#[async_trait]`:
///
/// ```
/// use async_trait::async_trait;
/// use eliteforge_agent_capability_macros::capability;
/// use eliteforge_agent_core::{AgentError, ContextContributor, ContextSourceKind, ContextInput, ContextBlock, Message};
///
/// #[derive(Default)]
/// struct ShopContext;
///
/// #[capability(pack = "shop")]
/// #[async_trait]
/// impl ContextContributor for ShopContext {
///     fn kind(&self) -> ContextSourceKind { ContextSourceKind::Instruction }
///     fn block_id(&self) -> &str { "shop.instructions" }
///     async fn contribute(&self, _: &ContextInput<'_>) -> Result<ContextBlock, AgentError> {
///         Ok(ContextBlock::instructions(self.block_id(), vec![Message::system("You assist the shop's support team.")]))
///     }
/// }
/// ```
#[proc_macro_attribute]
pub fn capability(attribute: TokenStream, item: TokenStream) -> TokenStream {
    match expand(attribute.into(), item.into()) {
        Ok(output) => output.into(),
        Err(error) => error.into_compile_error().into(),
    }
}

/// Declare a trait as an Agent extension point.
///
/// Implementations are contributed with `#[capability(...)]`. A plugin can
/// inspect the complete ordered set in its `ready` phase with
/// `context.extensions::<dyn Trait>()`.
#[proc_macro_attribute]
pub fn extension_point(attribute: TokenStream, item: TokenStream) -> TokenStream {
    match expand_extension_point(attribute.into(), item.into()) {
        Ok(output) => output.into(),
        Err(error) => error.into_compile_error().into(),
    }
}

/// Register a factory function whose return value is an ordinary `Plugin`.
///
/// This covers declarative registrations that need explicit binding metadata,
/// such as a model connection, a tool descriptor, or a value configuration
/// object. The function is still inert until its pack is selected.
#[proc_macro_attribute]
pub fn capability_factory(attribute: TokenStream, item: TokenStream) -> TokenStream {
    match expand_capability_factory(attribute.into(), item.into()) {
        Ok(output) => output.into(),
        Err(error) => error.into_compile_error().into(),
    }
}

/// Embed a capability crate's `skills/` directory in its published artifact.
///
/// `capability_skills!()` uses `skills/`; `capability_skills!("custom-skills")`
/// selects another package-relative directory. The returned source is passed to
/// `CapabilityPackBuilder::skills(...)`.
#[proc_macro]
pub fn capability_skills(input: TokenStream) -> TokenStream {
    match expand_capability_skills(input.into()) {
        Ok(output) => output.into(),
        Err(error) => error.into_compile_error().into(),
    }
}

/// Generate the build-script tracking block for a capability package's Skills
/// directory.
///
/// Call this inside `build.rs` so adding a file to the directory invalidates
/// the package build as well as changes to files that were already embedded.
#[proc_macro]
pub fn capability_skills_build(input: TokenStream) -> TokenStream {
    match expand_capability_skills_build(input.into()) {
        Ok(output) => output.into(),
        Err(error) => error.into_compile_error().into(),
    }
}

fn expand_capability_skills(input: TokenStream2) -> Result<TokenStream2> {
    let requested = if input.is_empty() {
        "skills".to_owned()
    } else {
        let path: LitStr = parse2(input)?;
        path.value()
    };
    let relative = FsPath::new(&requested);
    if relative.as_os_str().is_empty()
        || relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(Error::new(
            proc_macro2::Span::call_site(),
            "capability skills directory must be a normal package-relative path",
        ));
    }
    let manifest = std::env::var("CARGO_MANIFEST_DIR").map_err(|_| {
        Error::new(
            proc_macro2::Span::call_site(),
            "CARGO_MANIFEST_DIR is unavailable while expanding capability skills",
        )
    })?;
    let root = PathBuf::from(manifest).join(relative);
    let metadata = fs::metadata(&root).map_err(|error| {
        Error::new(
            proc_macro2::Span::call_site(),
            format!(
                "read capability skills directory `{}` failed: {error}",
                root.display()
            ),
        )
    })?;
    if !metadata.is_dir() {
        return Err(Error::new(
            proc_macro2::Span::call_site(),
            format!(
                "capability skills path `{}` is not a directory",
                root.display()
            ),
        ));
    }
    let mut files = Vec::new();
    collect_skill_files(&root, &root, &mut files)?;
    files.sort_by(|left, right| left.0.cmp(&right.0));
    if files.is_empty() {
        return Err(Error::new(
            proc_macro2::Span::call_site(),
            format!("capability skills directory `{}` is empty", root.display()),
        ));
    }
    let content_hash = capability_skills_hash(&files)?;
    let tracking_environment = capability_skills_tracking_environment(&requested);
    let entries = files.iter().map(|(relative, absolute)| {
        let absolute = absolute.to_string_lossy().to_string();
        quote! {
            ::eliteforge_agent_core::CapabilitySkillFile {
                path: #relative,
                contents: ::core::include_bytes!(#absolute),
            }
        }
    });
    Ok(quote! {{
        const _: &[u8] = ::core::include_bytes!(::core::env!(#tracking_environment));
        ::eliteforge_agent_core::CapabilitySkillSource::embedded(
            ::eliteforge_agent_core::EmbeddedCapabilitySkills::new(
                &[#(#entries),*],
                #content_hash,
            )
        )
    }})
}

fn expand_capability_skills_build(input: TokenStream2) -> Result<TokenStream2> {
    let requested = if input.is_empty() {
        "skills".to_owned()
    } else {
        let path: LitStr = parse2(input)?;
        path.value()
    };
    validate_capability_skills_directory(&requested)?;
    let tracking_environment = capability_skills_tracking_environment(&requested);
    let stamp_name = format!("{}.stamp", tracking_environment.to_ascii_lowercase());

    Ok(quote! {{
        fn collect_capability_skill_files(
            root: &::std::path::Path,
            directory: &::std::path::Path,
            files: &mut ::std::vec::Vec<::std::path::PathBuf>,
        ) {
            let mut entries = ::std::fs::read_dir(directory)
                .unwrap_or_else(|error| panic!("read capability skills directory `{}` failed: {error}", directory.display()))
                .collect::<::std::result::Result<::std::vec::Vec<_>, _>>()
                .unwrap_or_else(|error| panic!("read capability skills directory `{}` failed: {error}", directory.display()));
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                let path = entry.path();
                let metadata = ::std::fs::symlink_metadata(&path)
                    .unwrap_or_else(|error| panic!("inspect capability skill resource `{}` failed: {error}", path.display()));
                if metadata.file_type().is_symlink() {
                    panic!("capability skills cannot embed symbolic link `{}`", path.display());
                }
                if metadata.is_dir() {
                    collect_capability_skill_files(root, &path, files);
                } else if metadata.is_file() {
                    files.push(path);
                }
            }
            let _ = root;
        }

        let manifest = ::std::env::var("CARGO_MANIFEST_DIR")
            .expect("CARGO_MANIFEST_DIR is unavailable in capability skills build tracking");
        let root = ::std::path::PathBuf::from(manifest).join(#requested);
        if !root.is_dir() {
            panic!("capability skills path `{}` is not a directory", root.display());
        }
        println!("cargo:rerun-if-changed={}", root.display());
        let mut files = ::std::vec::Vec::new();
        collect_capability_skill_files(&root, &root, &mut files);
        files.sort_by(|left, right| left.strip_prefix(&root).unwrap().cmp(right.strip_prefix(&root).unwrap()));
        let mut stamp = ::std::vec::Vec::new();
        for file in files {
            let relative = file.strip_prefix(&root).expect("capability skill resource escaped its source directory");
            println!("cargo:rerun-if-changed={}", file.display());
            stamp.extend_from_slice(relative.to_string_lossy().as_bytes());
            stamp.push(0);
            stamp.extend_from_slice(&::std::fs::read(&file).unwrap_or_else(|error| panic!("read capability skill resource `{}` failed: {error}", file.display())));
            stamp.push(0);
        }
        let out_dir = ::std::path::PathBuf::from(::std::env::var("OUT_DIR").expect("OUT_DIR is unavailable in capability skills build tracking"));
        let stamp_path = out_dir.join(#stamp_name);
        ::std::fs::write(&stamp_path, stamp).unwrap_or_else(|error| panic!("write capability skills tracking stamp `{}` failed: {error}", stamp_path.display()));
        println!("cargo:rustc-env={}={}", #tracking_environment, stamp_path.display());
    }})
}

fn validate_capability_skills_directory(requested: &str) -> Result<()> {
    let relative = FsPath::new(requested);
    if relative.as_os_str().is_empty()
        || relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(Error::new(
            proc_macro2::Span::call_site(),
            "capability skills directory must be a normal package-relative path",
        ));
    }
    Ok(())
}

fn capability_skills_tracking_environment(requested: &str) -> String {
    let component = requested
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<String>();
    format!("ELITEFORGE_CAPABILITY_SKILLS_STAMP_{component}")
}

fn collect_skill_files(
    root: &FsPath,
    directory: &FsPath,
    output: &mut Vec<(String, PathBuf)>,
) -> Result<()> {
    let mut entries = fs::read_dir(directory)
        .map_err(|error| Error::new(proc_macro2::Span::call_site(), error.to_string()))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| Error::new(proc_macro2::Span::call_site(), error.to_string()))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| Error::new(proc_macro2::Span::call_site(), error.to_string()))?;
        if metadata.file_type().is_symlink() {
            return Err(Error::new(
                proc_macro2::Span::call_site(),
                format!(
                    "capability skills cannot embed symbolic link `{}`",
                    path.display()
                ),
            ));
        }
        if metadata.is_dir() {
            collect_skill_files(root, &path, output)?;
            continue;
        }
        if !metadata.is_file() {
            continue;
        }
        let relative = path.strip_prefix(root).map_err(|_| {
            Error::new(
                proc_macro2::Span::call_site(),
                "capability skill resource escapes its source directory",
            )
        })?;
        let relative = relative
            .components()
            .map(|component| match component {
                std::path::Component::Normal(value) => {
                    value.to_str().map(str::to_owned).ok_or_else(|| {
                        Error::new(
                            proc_macro2::Span::call_site(),
                            "capability skill resource path is not valid UTF-8",
                        )
                    })
                }
                _ => Err(Error::new(
                    proc_macro2::Span::call_site(),
                    "capability skill resource path is not normal",
                )),
            })
            .collect::<Result<Vec<_>>>()?
            .join("/");
        output.push((relative, path));
    }
    Ok(())
}

fn capability_skills_hash(files: &[(String, PathBuf)]) -> Result<String> {
    let mut hash = Sha256::new();
    for (relative, path) in files {
        hash.update(relative.as_bytes());
        hash.update([0_u8]);
        let bytes = fs::read(path).map_err(|error| {
            Error::new(
                proc_macro2::Span::call_site(),
                format!(
                    "read capability skill resource `{}` failed: {error}",
                    path.display()
                ),
            )
        })?;
        hash.update(bytes);
        hash.update([0_u8]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

#[derive(Default)]
struct CapabilityFactoryArguments {
    pack: Option<LitStr>,
    id: Option<LitStr>,
    instance: Option<LitStr>,
    depends_on: Option<Vec<LitStr>>,
    config: Option<Type>,
    skills_runtime: bool,
}

impl CapabilityFactoryArguments {
    fn parse(input: TokenStream2) -> Result<Self> {
        if input.is_empty() {
            return Err(Error::new(
                proc_macro2::Span::call_site(),
                "missing required `pack = \"...\"` argument",
            ));
        }
        let mut arguments = Self::default();
        syn::meta::parser(|meta| {
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
                arguments.depends_on = Some(parse_dependencies(meta.value()?.parse::<ExprArray>().map_err(
                    |_| meta.error("`depends_on` must be an array of string literals, for example [\"storage\"]"),
                )?)?);
            } else if meta.path.is_ident("config") {
                if arguments.config.is_some() {
                    return Err(meta.error("duplicate `config` argument"));
                }
                arguments.config = Some(meta.value()?.parse()?);
            } else if meta.path.is_ident("skills_runtime") {
                if arguments.skills_runtime {
                    return Err(meta.error("duplicate `skills_runtime` argument"));
                }
                if meta.input.peek(syn::Token![=]) {
                    return Err(meta.error("`skills_runtime` does not accept a value"));
                }
                arguments.skills_runtime = true;
            } else {
                return Err(meta.error(
                    "unsupported capability-factory argument; expected `pack`, `id`, `instance`, `depends_on`, `config`, or `skills_runtime`",
                ));
            }
            Ok(())
        })
        .parse2(input)?;

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
        Ok(arguments)
    }
}

fn expand_capability_factory(attribute: TokenStream2, item: TokenStream2) -> Result<TokenStream2> {
    let arguments = CapabilityFactoryArguments::parse(attribute)?;
    let factory: ItemFn = parse2(item)?;
    if factory.sig.asyncness.is_some() {
        return Err(Error::new_spanned(
            factory.sig.asyncness,
            "capability factories must be synchronous; initialize asynchronous resources in Plugin::install",
        ));
    }
    if !factory.sig.generics.params.is_empty() {
        return Err(Error::new_spanned(
            &factory.sig.generics,
            "capability factories cannot have generic parameters",
        ));
    }
    let ReturnType::Type(_, return_type) = &factory.sig.output else {
        return Err(Error::new_spanned(
            &factory.sig,
            "capability factory must return a concrete Plugin type",
        ));
    };
    if matches!(return_type.as_ref(), Type::ImplTrait(_)) {
        return Err(Error::new_spanned(
            return_type,
            "capability factory must return a concrete Plugin type, not `impl Plugin`",
        ));
    }

    let function_name = &factory.sig.ident;
    let provides_skills_runtime = arguments.skills_runtime || is_skills_plugin_type(return_type);
    let pack = arguments.pack.expect("pack was validated");
    let default_id = quote!(concat!(#pack, "::", module_path!(), "::", stringify!(#function_name)));
    let id = arguments.id.map_or(default_id, |value| quote!(#value));
    let default_instance =
        quote!(concat!(#pack, "::", module_path!(), "::", stringify!(#function_name)));
    let default_instance_selected = arguments.instance.is_none();
    let instance = arguments
        .instance
        .map_or(default_instance, |value| quote!(#value));
    let dependencies = arguments.depends_on.unwrap_or_default();

    let (configuration_type, factory_body, type_identity) = match arguments.config {
        Some(config) => {
            if factory.sig.inputs.len() != 1
                || !matches!(factory.sig.inputs.first(), Some(FnArg::Typed(argument)) if matches!(argument.ty.as_ref(), Type::Reference(_)))
            {
                return Err(Error::new_spanned(
                    &factory.sig.inputs,
                    "a configured capability factory must accept exactly one `&Config` argument",
                ));
            }
            let signature = quote!(concat!(
                #pack,
                "::",
                module_path!(),
                "::",
                stringify!(#function_name),
                "::config=",
                stringify!(#config)
            ));
            let body = quote! {
                let configuration = configuration.downcast_ref::<#config>().ok_or_else(|| {
                    ::eliteforge_agent_core::CapabilityError::configuration_type(
                        #id,
                        stringify!(#config),
                    )
                })?;
                let implementation: #return_type = #function_name(configuration);
                Ok(::std::sync::Arc::new(implementation))
            };
            let identity = quote!(.with_type_identity::<#return_type, #config, _>(
                #function_name,
                #default_instance_selected,
            ));
            (signature, body, identity)
        }
        None => {
            if !factory.sig.inputs.is_empty() {
                return Err(Error::new_spanned(
                    &factory.sig.inputs,
                    "an unconfigured capability factory must not accept arguments; add `config = YourConfig` for one `&Config` argument",
                ));
            }
            let signature = quote!(concat!(
                #pack,
                "::",
                module_path!(),
                "::",
                stringify!(#function_name),
                "::default"
            ));
            let body = quote! {
                let _ = configuration;
                let implementation: #return_type = #function_name();
                Ok(::std::sync::Arc::new(implementation))
            };
            let identity = quote!(.with_type_identity::<#return_type, (), _>(
                #function_name,
                #default_instance_selected,
            ));
            (signature, body, identity)
        }
    };

    let skills_runtime = provides_skills_runtime.then(|| quote!(.with_provided_skills_runtime()));

    Ok(quote! {
        #factory

        const _: () = {
            #[doc(hidden)]
            fn descriptor_factory(
                configuration: &::eliteforge_agent_core::CapabilityConfig,
            ) -> ::core::result::Result<
                ::eliteforge_agent_core::CapabilityInstance,
                ::eliteforge_agent_core::CapabilityError,
            > {
                #factory_body
            }

            #[doc(hidden)]
            fn descriptor_adapter(
                instance: ::eliteforge_agent_core::CapabilityInstance,
            ) -> ::core::result::Result<
                ::std::sync::Arc<dyn ::eliteforge_agent_core::Plugin>,
                ::eliteforge_agent_core::CapabilityError,
            > {
                let implementation = ::std::sync::Arc::downcast::<#return_type>(instance).map_err(|_| {
                    ::eliteforge_agent_core::CapabilityError::instance_type(
                        #id,
                        stringify!(#return_type),
                    )
                })?;
                let implementation: ::std::sync::Arc<dyn ::eliteforge_agent_core::Plugin> = implementation;
                Ok(implementation)
            }

            ::eliteforge_agent_core::__private::inventory::submit! {
                ::eliteforge_agent_core::CapabilityDescriptor::new(
                    #pack,
                    #id,
                    #instance,
                    #configuration_type,
                    &[#(#dependencies),*],
                    descriptor_factory,
                    descriptor_adapter,
                ) #type_identity #skills_runtime
            }
        };
    })
}

fn is_skills_plugin_type(ty: &Type) -> bool {
    matches!(
        ty,
        Type::Path(path) if path
            .path
            .segments
            .last()
            .is_some_and(|segment| segment.ident == "SkillsPlugin")
    )
}

#[derive(Default)]
struct ExtensionPointArguments {
    single: bool,
    select: bool,
    adapter: Option<Path>,
    contribute: bool,
}

impl ExtensionPointArguments {
    fn parse(input: TokenStream2) -> Result<Self> {
        let mut arguments = Self::default();
        if input.is_empty() {
            return Ok(arguments);
        }
        syn::meta::parser(|meta| {
            if meta.path.is_ident("single") {
                if arguments.single {
                    return Err(meta.error("duplicate `single` argument"));
                }
                if meta.input.peek(syn::Token![=]) {
                    return Err(meta.error("`single` does not accept a value"));
                }
                arguments.single = true;
            } else if meta.path.is_ident("select") {
                if arguments.select { return Err(meta.error("duplicate `select` argument")); }
                if meta.input.peek(syn::Token![=]) { return Err(meta.error("`select` does not accept a value")); }
                arguments.select = true;
            } else if meta.path.is_ident("adapter") {
                if arguments.adapter.is_some() {
                    return Err(meta.error("duplicate `adapter` argument"));
                }
                arguments.adapter = Some(meta.value()?.parse()?);
            } else if meta.path.is_ident("contribute") {
                if arguments.contribute {
                    return Err(meta.error("duplicate `contribute` argument"));
                }
                if meta.input.peek(syn::Token![=]) {
                    return Err(meta.error("`contribute` does not accept a value"));
                }
                arguments.contribute = true;
            } else {
                return Err(meta.error(
                    "unsupported extension-point argument; expected `single`, `select`, `adapter = Type`, or `contribute`",
                ));
            }
            Ok(())
        })
        .parse2(input)?;
        if arguments.single && arguments.select {
            return Err(Error::new(
                proc_macro2::Span::call_site(),
                "`single` and `select` cannot be combined",
            ));
        }
        if arguments.select && arguments.contribute {
            return Err(Error::new(
                proc_macro2::Span::call_site(),
                "`select` and `contribute` cannot be combined",
            ));
        }
        if arguments.contribute && arguments.adapter.is_none() {
            return Err(Error::new(
                proc_macro2::Span::call_site(),
                "`contribute` requires `adapter = Type`",
            ));
        }
        Ok(arguments)
    }
}

fn expand_extension_point(attribute: TokenStream2, item: TokenStream2) -> Result<TokenStream2> {
    let arguments = ExtensionPointArguments::parse(attribute)?;
    let interface: ItemTrait = parse2(item)?;
    if !interface.generics.params.is_empty() {
        return Err(Error::new_spanned(
            &interface.generics,
            "extension-point traits cannot have generic parameters; declare a concrete extension interface",
        ));
    }

    let identifier = &interface.ident;
    let visibility = &interface.vis;
    let storage = format_ident!("__{}Extension", identifier);
    let multiplicity = if arguments.single {
        quote!(::eliteforge_agent_core::ExtensionMultiplicity::Single)
    } else {
        quote!(::eliteforge_agent_core::ExtensionMultiplicity::Multiple)
    };
    let binding = if arguments.single {
        quote!(::eliteforge_agent_core::ExtensionBinding::Shared)
    } else if arguments.select {
        quote!(::eliteforge_agent_core::ExtensionBinding::SelectOne)
    } else {
        quote!(::eliteforge_agent_core::ExtensionBinding::Contributions)
    };
    let capability_adapter = match arguments.adapter {
        Some(adapter) if arguments.select => quote! {
            fn into_plugin(id: &str, implementation: ::std::sync::Arc<Self>) -> ::std::sync::Arc<dyn ::eliteforge_agent_core::Plugin> {
                ::std::sync::Arc::new(#adapter::named_arc(id, implementation))
            }
        },
        Some(adapter) if arguments.contribute => quote! {
            fn into_plugin(
                id: &str,
                implementation: ::std::sync::Arc<Self>,
            ) -> ::std::sync::Arc<dyn ::eliteforge_agent_core::Plugin> {
                ::std::sync::Arc::new(#adapter::from_arc(id, implementation))
            }
        },
        Some(adapter) => quote! {
            fn into_plugin(
                _id: &str,
                implementation: ::std::sync::Arc<Self>,
            ) -> ::std::sync::Arc<dyn ::eliteforge_agent_core::Plugin> {
                ::std::sync::Arc::new(#adapter::from_arc(implementation))
            }
        },
        None => quote! {
            fn into_plugin(
                id: &str,
                implementation: ::std::sync::Arc<Self>,
            ) -> ::std::sync::Arc<dyn ::eliteforge_agent_core::Plugin> {
                ::std::sync::Arc::new(
                    ::eliteforge_agent_core::ExtensionPlugin::<dyn #identifier>::from_arc(
                        id,
                        implementation,
                    ),
                )
            }
        },
    };

    Ok(quote! {
        #interface

        #[doc(hidden)]
        #[allow(non_camel_case_types)]
        #visibility struct #storage(::std::sync::Arc<dyn #identifier>);

        impl ::eliteforge_agent_core::ExtensionPoint for dyn #identifier {
            type Stored = #storage;

            const MULTIPLICITY: ::eliteforge_agent_core::ExtensionMultiplicity = #multiplicity;
            const BINDING: ::eliteforge_agent_core::ExtensionBinding = #binding;

            fn into_stored(value: ::std::sync::Arc<Self>) -> Self::Stored {
                #storage(value)
            }

            fn from_stored(value: &Self::Stored) -> ::std::sync::Arc<Self> {
                value.0.clone()
            }

            fn name() -> &'static str {
                concat!(module_path!(), "::", stringify!(#identifier))
            }
        }

        impl ::eliteforge_agent_core::CapabilityExtensionAdapter for dyn #identifier {
            #capability_adapter
        }
    })
}

#[derive(Default)]
struct CapabilityArguments {
    pack: Option<LitStr>,
    id: Option<LitStr>,
    instance: Option<LitStr>,
    depends_on: Option<Vec<LitStr>>,
    config: Option<Type>,
    factory: Option<Path>,
    adapter: Option<Path>,
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
            } else if meta.path.is_ident("adapter") {
                if arguments.adapter.is_some() {
                    return Err(meta.error("duplicate `adapter` argument"));
                }
                let adapter: Path = meta.value()?.parse()?;
                if adapter.segments.len() < 2 {
                    return Err(meta.error(
                        "`adapter` must be an associated function path such as Type::into_plugin",
                    ));
                }
                arguments.adapter = Some(adapter);
            } else {
                return Err(meta.error(
                    "unsupported capability argument; expected `pack`, `id`, `instance`, `depends_on`, `config`, `factory`, or `adapter`",
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
    ToolProvider,
    EventSink,
    ContextContributor,
    ModelMiddleware,
    ToolMiddleware,
    RunMiddleware,
    ToolPolicy,
    RunLifecycleHook,
    RunOutputFactory,
    ConversationStore,
    LongTermMemory,
    AgentLoop,
    ContextAssembler,
    ContextBudgetPolicy,
    ContextReducer,
    StateStore,
    InteractionStore,
    SummaryStore,
    ArtifactStore,
    ModelSelectionStrategy,
    ChatModel,
    SkillProvider,
    Plugin,
    Extension,
}

impl CapabilityInterface {
    fn parse(path: &Path) -> Result<Self> {
        let Some(last_segment) = path.segments.last() else {
            return Err(Error::new_spanned(
                path,
                "capability requires a supported trait implementation",
            ));
        };
        match last_segment.ident.to_string().as_str() {
            "ToolProvider" => Ok(Self::ToolProvider),
            "EventSink" => Ok(Self::EventSink),
            "ContextContributor" => Ok(Self::ContextContributor),
            "ModelMiddleware" => Ok(Self::ModelMiddleware),
            "ToolMiddleware" => Ok(Self::ToolMiddleware),
            "RunMiddleware" => Ok(Self::RunMiddleware),
            "ToolPolicy" => Ok(Self::ToolPolicy),
            "RunLifecycleHook" => Ok(Self::RunLifecycleHook),
            "RunOutputFactory" => Ok(Self::RunOutputFactory),
            "ConversationStore" => Ok(Self::ConversationStore),
            "LongTermMemory" => Ok(Self::LongTermMemory),
            "AgentLoop" => Ok(Self::AgentLoop),
            "ContextAssembler" => Ok(Self::ContextAssembler),
            "ContextBudgetPolicy" => Ok(Self::ContextBudgetPolicy),
            "ContextReducer" => Ok(Self::ContextReducer),
            "StateStore" => Ok(Self::StateStore),
            "InteractionStore" => Ok(Self::InteractionStore),
            "SummaryStore" => Ok(Self::SummaryStore),
            "ArtifactStore" => Ok(Self::ArtifactStore),
            "ModelSelectionStrategy" => Ok(Self::ModelSelectionStrategy),
            "ChatModel" => Ok(Self::ChatModel),
            "SkillProvider" => Ok(Self::SkillProvider),
            "Plugin" => Ok(Self::Plugin),
            _ => Ok(Self::Extension),
        }
    }

    fn adapter(self) -> Option<TokenStream2> {
        let adapter = match self {
            Self::ToolProvider => quote!(::eliteforge_agent_core::ToolProviderPlugin),
            Self::EventSink => quote!(::eliteforge_agent_core::EventPlugin),
            Self::ContextContributor => quote!(::eliteforge_agent_core::ContextContributorPlugin),
            Self::ModelMiddleware => quote!(::eliteforge_agent_core::ModelMiddlewarePlugin),
            Self::ToolMiddleware => quote!(::eliteforge_agent_core::ToolMiddlewarePlugin),
            Self::RunMiddleware => quote!(::eliteforge_agent_core::RunMiddlewarePlugin),
            Self::ToolPolicy => quote!(::eliteforge_agent_core::ToolPolicyPlugin),
            Self::RunLifecycleHook => quote!(::eliteforge_agent_core::RunLifecycleHookPlugin),
            Self::RunOutputFactory => quote!(::eliteforge_agent_core::RunOutputPlugin),
            Self::ConversationStore => quote!(::eliteforge_agent_core::ConversationStorePlugin),
            Self::LongTermMemory => quote!(::eliteforge_agent_core::LongTermMemoryPlugin),
            Self::AgentLoop => quote!(::eliteforge_agent_core::AgentLoopPlugin),
            Self::ContextAssembler => quote!(::eliteforge_agent_core::ContextAssemblerPlugin),
            Self::ContextBudgetPolicy => {
                quote!(::eliteforge_agent_core::ContextBudgetPolicyPlugin)
            }
            Self::ContextReducer => quote!(::eliteforge_agent_core::ContextReducerPlugin),
            Self::StateStore => quote!(::eliteforge_agent_core::StateStorePlugin),
            Self::InteractionStore => quote!(::eliteforge_agent_core::InteractionStorePlugin),
            Self::SummaryStore => quote!(::eliteforge_agent_context::SummaryStorePlugin),
            Self::ArtifactStore => quote!(::eliteforge_agent_context::ArtifactStorePlugin),
            Self::ModelSelectionStrategy => quote!(::eliteforge_agent_core::ModelStrategyPlugin),
            Self::ChatModel => quote!(::eliteforge_agent_core::ChatModelPlugin),
            Self::SkillProvider => quote!(::eliteforge_agent_core::ExtensionPlugin),
            Self::Plugin => return None,
            Self::Extension => quote!(::eliteforge_agent_core::ExtensionPlugin),
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
    let explicit_adapter = arguments.adapter;

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

    let adapter_body = match (explicit_adapter, interface.adapter()) {
        (Some(adapter), _) => quote! {
            let implementation = ::std::sync::Arc::downcast::<#self_ty>(instance).map_err(|_| {
                ::eliteforge_agent_core::CapabilityError::instance_type(
                    #id,
                    stringify!(#self_ty),
                )
            })?;
            let implementation: ::std::sync::Arc<dyn #trait_path> = implementation;
            Ok(::std::sync::Arc::new(#adapter(implementation)))
        },
        (None, Some(_adapter))
            if matches!(
                interface,
                CapabilityInterface::Extension | CapabilityInterface::SkillProvider
            ) =>
        {
            quote! {
                let implementation = ::std::sync::Arc::downcast::<#self_ty>(instance).map_err(|_| {
                    ::eliteforge_agent_core::CapabilityError::instance_type(
                        #id,
                        stringify!(#self_ty),
                    )
                })?;
                let implementation: ::std::sync::Arc<dyn #trait_path> = implementation;
                Ok(<dyn #trait_path as ::eliteforge_agent_core::CapabilityExtensionAdapter>::into_plugin(
                    #id,
                    implementation,
                ))
            }
        }
        (None, Some(adapter)) if matches!(interface, CapabilityInterface::ChatModel) => quote! {
            let implementation = ::std::sync::Arc::downcast::<#self_ty>(instance).map_err(|_| {
                ::eliteforge_agent_core::CapabilityError::instance_type(
                    #id,
                    stringify!(#self_ty),
                )
            })?;
            let implementation: ::std::sync::Arc<dyn #trait_path> = implementation;
            Ok(::std::sync::Arc::new(#adapter::from_arc(implementation)))
        },
        (None, Some(adapter))
            if matches!(
                interface,
                CapabilityInterface::ConversationStore
                    | CapabilityInterface::LongTermMemory
                    | CapabilityInterface::AgentLoop
                    | CapabilityInterface::ContextAssembler
                    | CapabilityInterface::ContextBudgetPolicy
                    | CapabilityInterface::ContextReducer
                    | CapabilityInterface::StateStore
                    | CapabilityInterface::InteractionStore
                    | CapabilityInterface::SummaryStore
                    | CapabilityInterface::ArtifactStore
                    | CapabilityInterface::ModelSelectionStrategy
            ) =>
        {
            quote! {
                let implementation = ::std::sync::Arc::downcast::<#self_ty>(instance).map_err(|_| {
                    ::eliteforge_agent_core::CapabilityError::instance_type(
                        #id,
                        stringify!(#self_ty),
                    )
                })?;
                let implementation: ::std::sync::Arc<dyn #trait_path> = implementation;
                Ok(::std::sync::Arc::new(#adapter::from_arc(implementation)))
            }
        }
        (None, Some(adapter)) => quote! {
            let implementation = ::std::sync::Arc::downcast::<#self_ty>(instance).map_err(|_| {
                ::eliteforge_agent_core::CapabilityError::instance_type(
                    #id,
                    stringify!(#self_ty),
                )
            })?;
            let implementation: ::std::sync::Arc<dyn #trait_path> = implementation;
            Ok(::std::sync::Arc::new(#adapter::from_arc(#id, implementation)))
        },
        (None, None) => quote! {
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

    let skills_runtime = matches!(interface, CapabilityInterface::SkillProvider)
        .then(|| quote!(.with_skills_runtime()));

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
                ) #type_identity #skills_runtime
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

    fn instruction_source_impl() -> TokenStream2 {
        quote! {
                #[async_trait]
                impl ContextContributor for ShopContext {
        fn kind(&self) -> ContextSourceKind { ContextSourceKind::Instruction }
        fn block_id(&self) -> &str { std::any::type_name::<Self>() }
        async fn contribute(&self, _input: &ContextInput<'_>) -> Result<ContextBlock, AgentError> {
            Ok(ContextBlock::instructions(self.block_id(), Vec::new()))
        }
                }
            }
    }

    #[test]
    fn expands_async_trait_impl_and_static_registration() {
        let output = expand(
            quote!(pack = "shop", depends_on = ["storage", "auth"]),
            instruction_source_impl(),
        )
        .expect("valid capability expands");
        let output = output.to_string();

        assert!(output.contains("async_trait"));
        assert!(output.contains("inventory :: submit"));
        assert!(output.contains("ContextContributorPlugin :: from_arc"));
        assert!(output.contains("CapabilityDescriptor :: new"));
        assert!(output.contains("module_path"));
    }

    #[test]
    fn requires_pack() {
        let error = expand(quote!(), instruction_source_impl()).expect_err("pack is required");
        assert!(
            error.to_string().contains("missing required `pack ="),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn requires_config_and_factory_together() {
        let config_only = expand(
            quote!(pack = "shop", config = ShopConfig),
            instruction_source_impl(),
        )
        .expect_err("config alone is invalid");
        assert!(
            config_only
                .to_string()
                .contains("`config` and `factory` must be specified together")
        );

        let factory_only = expand(
            quote!(pack = "shop", factory = ShopContext::from_config),
            instruction_source_impl(),
        )
        .expect_err("factory alone is invalid");
        assert!(
            factory_only
                .to_string()
                .contains("`config` and `factory` must be specified together")
        );
    }

    #[test]
    fn routes_unknown_traits_to_the_extension_adapter() {
        let output = expand(
            quote!(pack = "shop"),
            quote!(impl PaymentRule for ShopContext {}),
        )
        .expect("extension traits should use the generic adapter")
        .to_string();
        assert!(output.contains("CapabilityExtensionAdapter"));
    }

    #[test]
    fn rejects_non_string_dependency() {
        let error = expand(
            quote!(pack = "shop", depends_on = ["storage", dependency_id]),
            instruction_source_impl(),
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
            instruction_source_impl(),
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
            ("ToolProvider", Some("ToolProviderPlugin")),
            ("EventSink", Some("EventPlugin")),
            ("ContextContributor", Some("ContextContributorPlugin")),
            ("ModelMiddleware", Some("ModelMiddlewarePlugin")),
            ("ToolMiddleware", Some("ToolMiddlewarePlugin")),
            ("RunMiddleware", Some("RunMiddlewarePlugin")),
            ("ToolPolicy", Some("ToolPolicyPlugin")),
            ("RunLifecycleHook", Some("RunLifecycleHookPlugin")),
            ("RunOutputFactory", Some("RunOutputPlugin")),
            ("ConversationStore", Some("ConversationStorePlugin")),
            ("LongTermMemory", Some("LongTermMemoryPlugin")),
            ("AgentLoop", Some("AgentLoopPlugin")),
            ("ContextAssembler", Some("ContextAssemblerPlugin")),
            ("ContextBudgetPolicy", Some("ContextBudgetPolicyPlugin")),
            ("ContextReducer", Some("ContextReducerPlugin")),
            ("StateStore", Some("StateStorePlugin")),
            ("InteractionStore", Some("InteractionStorePlugin")),
            ("SummaryStore", Some("SummaryStorePlugin")),
            ("ArtifactStore", Some("ArtifactStorePlugin")),
            ("ModelSelectionStrategy", Some("ModelStrategyPlugin")),
            ("ChatModel", Some("ChatModelPlugin")),
            ("SkillProvider", Some("ExtensionPlugin")),
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
    fn skill_provider_declarations_request_the_aggregate_skills_runtime() {
        let output = expand(
            quote!(pack = "skills"),
            quote! {
                impl SkillProvider for PackageSkills {
                    fn release(&self, _: &RunContext) {}
                }
            },
        )
        .expect("skill provider capability expands")
        .to_string();

        assert!(output.contains("with_skills_runtime"));
        assert!(output.contains("CapabilityExtensionAdapter"));
    }

    #[test]
    fn skills_build_tracking_emits_a_directory_watch_and_compile_stamp() {
        let output = expand_capability_skills_build(quote!("package-skills"))
            .expect("build tracking expands")
            .to_string();

        assert!(output.contains("rerun"));
        assert!(output.contains("ELITEFORGE_CAPABILITY_SKILLS_STAMP_"));
        assert!(output.contains("rustc"));
    }

    #[test]
    fn default_shared_identity_does_not_include_the_trait() {
        let pack = syn::parse_str::<LitStr>("\"shop\"").expect("test pack parses");
        let implementation =
            syn::parse_str::<Type>("SharedCapability").expect("test implementation type parses");

        let instance = default_instance_key(&pack, &implementation).to_string();
        let factory_signature = default_factory_signature(&pack, &implementation).to_string();

        assert!(!instance.contains("ContextContributor"));
        assert!(!instance.contains("EventSink"));
        assert!(!factory_signature.contains("ContextContributor"));
        assert!(!factory_signature.contains("EventSink"));
    }
}
