// Copyright 2024 New Vector Ltd.
// Copyright 2022-2024 The Matrix.org Foundation C.I.C.
//
// SPDX-License-Identifier: AGPL-3.0-only
// Please see LICENSE in the repository root for full details.

use std::sync::Arc;

use axum::{
    Form,
    extract::{Path, State},
    response::{Html, IntoResponse, Response},
};
use axum_extra::typed_header::TypedHeader;
use hyper::StatusCode;
use mas_axum_utils::{
    FancyError, SessionInfoExt,
    cookies::CookieJar,
    csrf::{CsrfExt, ProtectedForm},
    sentry::SentryEventID,
};
use mas_data_model::{User, UserAgent};
use mas_jose::jwt::Jwt;
use mas_matrix::HomeserverConnection;
use mas_policy::Policy;
use mas_router::UrlBuilder;
use mas_storage::{
    BoxClock, BoxRepository, BoxRng, RepositoryAccess,
    queue::{ProvisionUserJob, QueueJobRepositoryExt as _},
    upstream_oauth2::{UpstreamOAuthLinkRepository, UpstreamOAuthSessionRepository},
    user::{BrowserSessionRepository, UserEmailRepository, UserRepository},
};
use mas_templates::{
    ErrorContext, FieldError, FormError, TemplateContext, Templates, ToFormState,
    UpstreamExistingLinkContext, UpstreamRegister, UpstreamSuggestLink,
};
use minijinja::Environment;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::warn;
use ulid::Ulid;

use super::{
    UpstreamSessionsCookie,
    template::{AttributeMappingContext, environment},
};
use crate::{
    BoundActivityTracker, PreferredLanguage, SiteConfig, impl_from_error_for_route,
    views::shared::OptionalPostAuthAction,
};

const DEFAULT_LOCALPART_TEMPLATE: &str = "{{ user.preferred_username }}";
const DEFAULT_DISPLAYNAME_TEMPLATE: &str = "{{ user.name }}";
const DEFAULT_EMAIL_TEMPLATE: &str = "{{ user.email }}";

#[derive(Debug, Error)]
pub(crate) enum RouteError {
    /// Couldn't find the link specified in the URL
    #[error("Link not found")]
    LinkNotFound,

    /// Couldn't find the session on the link
    #[error("Session not found")]
    SessionNotFound,

    /// Couldn't find the user
    #[error("User not found")]
    UserNotFound,

    /// Couldn't find upstream provider
    #[error("Upstream provider not found")]
    ProviderNotFound,

    /// Required attribute rendered to an empty string
    #[error("Template {template:?} rendered to an empty string")]
    RequiredAttributeEmpty { template: String },

    /// Required claim was missing in `id_token`
    #[error(
        "Template {template:?} could not be rendered from the upstream provider's response for required claim"
    )]
    RequiredAttributeRender {
        template: String,

        #[source]
        source: minijinja::Error,
    },

    /// Session was already consumed
    #[error("Session already consumed")]
    SessionConsumed,

    #[error("Missing session cookie")]
    MissingCookie,

    #[error("Invalid form action")]
    InvalidFormAction,

    #[error("Homeserver connection error")]
    HomeserverConnection(#[source] anyhow::Error),

    #[error(transparent)]
    Internal(Box<dyn std::error::Error + Send + Sync + 'static>),
}

impl_from_error_for_route!(mas_templates::TemplateError);
impl_from_error_for_route!(mas_axum_utils::csrf::CsrfError);
impl_from_error_for_route!(super::cookie::UpstreamSessionNotFound);
impl_from_error_for_route!(mas_storage::RepositoryError);
impl_from_error_for_route!(mas_policy::EvaluationError);
impl_from_error_for_route!(mas_jose::jwt::JwtDecodeError);

impl IntoResponse for RouteError {
    fn into_response(self) -> axum::response::Response {
        let event_id = sentry::capture_error(&self);
        let response = match self {
            Self::LinkNotFound => (StatusCode::NOT_FOUND, "Link not found").into_response(),
            Self::Internal(e) => FancyError::from(e).into_response(),
            e => FancyError::from(e).into_response(),
        };

        (SentryEventID::from(event_id), response).into_response()
    }
}

/// Utility function to render an attribute template.
///
/// # Parameters
///
/// * `environment` - The minijinja environment to use to render the template
/// * `template` - The template to use to render the claim
/// * `required` - Whether the attribute is required or not
///
/// # Errors
///
/// Returns an error if the attribute is required but fails to render or is
/// empty
fn render_attribute_template(
    environment: &Environment,
    template: &str,
    context: &minijinja::Value,
    required: bool,
) -> Result<Option<String>, RouteError> {
    match environment.render_str(template, context) {
        Ok(value) if value.is_empty() => {
            if required {
                return Err(RouteError::RequiredAttributeEmpty {
                    template: template.to_owned(),
                });
            }

            Ok(None)
        }

        Ok(value) => Ok(Some(value)),

        Err(source) => {
            if required {
                return Err(RouteError::RequiredAttributeRender {
                    template: template.to_owned(),
                    source,
                });
            }

            tracing::warn!(error = &source as &dyn std::error::Error, %template, "Error while rendering template");
            Ok(None)
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "lowercase", tag = "action")]
pub(crate) enum FormData {
    Register {
        #[serde(default)]
        username: Option<String>,
        #[serde(default)]
        import_email: Option<String>,
        #[serde(default)]
        import_display_name: Option<String>,
        #[serde(default)]
        accept_terms: Option<String>,
    },
    Link,
}

impl ToFormState for FormData {
    type Field = mas_templates::UpstreamRegisterFormField;
}

#[tracing::instrument(
    name = "handlers.upstream_oauth2.link.get",
    fields(upstream_oauth_link.id = %link_id),
    skip_all,
    err,
)]
pub(crate) async fn get(
    mut rng: BoxRng,
    clock: BoxClock,
    mut repo: BoxRepository,
    mut policy: Policy,
    PreferredLanguage(locale): PreferredLanguage,
    State(templates): State<Templates>,
    State(url_builder): State<UrlBuilder>,
    State(homeserver): State<Arc<dyn HomeserverConnection>>,
    cookie_jar: CookieJar,
    activity_tracker: BoundActivityTracker,
    user_agent: Option<TypedHeader<headers::UserAgent>>,
    Path(link_id): Path<Ulid>,
) -> Result<impl IntoResponse, RouteError> {
    let user_agent = user_agent.map(|ua| UserAgent::parse(ua.as_str().to_owned()));
    let sessions_cookie = UpstreamSessionsCookie::load(&cookie_jar);
    let (session_id, post_auth_action) = sessions_cookie
        .lookup_link(link_id)
        .map_err(|_| RouteError::MissingCookie)?;

    let post_auth_action = OptionalPostAuthAction {
        post_auth_action: post_auth_action.cloned(),
    };

    let link = repo
        .upstream_oauth_link()
        .lookup(link_id)
        .await?
        .ok_or(RouteError::LinkNotFound)?;

    let upstream_session = repo
        .upstream_oauth_session()
        .lookup(session_id)
        .await?
        .ok_or(RouteError::SessionNotFound)?;

    // This checks that we're in a browser session which is allowed to consume this
    // link: the upstream auth session should have been started in this browser.
    if upstream_session.link_id() != Some(link.id) {
        return Err(RouteError::SessionNotFound);
    }

    if upstream_session.is_consumed() {
        return Err(RouteError::SessionConsumed);
    }

    let (user_session_info, cookie_jar) = cookie_jar.session_info();
    let (csrf_token, mut cookie_jar) = cookie_jar.csrf_token(&clock, &mut rng);
    let maybe_user_session = user_session_info.load_session(&mut repo).await?;

    let response = match (maybe_user_session, link.user_id) {
        (Some(session), Some(user_id)) if session.user.id == user_id => {
            // Session already linked, and link matches the currently logged
            // user. Mark the session as consumed and renew the authentication.
            let upstream_session = repo
                .upstream_oauth_session()
                .consume(&clock, upstream_session)
                .await?;

            repo.browser_session()
                .authenticate_with_upstream(&mut rng, &clock, &session, &upstream_session)
                .await?;

            cookie_jar = cookie_jar.set_session(&session);

            repo.save().await?;

            post_auth_action.go_next(&url_builder).into_response()
        }

        (Some(user_session), Some(user_id)) => {
            // Session already linked, but link doesn't match the currently
            // logged user. Suggest logging out of the current user
            // and logging in with the new one
            let user = repo
                .user()
                .lookup(user_id)
                .await?
                // XXX: is that right?
                .filter(User::is_valid)
                .ok_or(RouteError::UserNotFound)?;

            let ctx = UpstreamExistingLinkContext::new(user)
                .with_session(user_session)
                .with_csrf(csrf_token.form_value())
                .with_language(locale);

            Html(templates.render_upstream_oauth2_link_mismatch(&ctx)?).into_response()
        }

        (Some(user_session), None) => {
            // Session not linked, but user logged in: suggest linking account
            let ctx = UpstreamSuggestLink::new(&link)
                .with_session(user_session)
                .with_csrf(csrf_token.form_value())
                .with_language(locale);

            Html(templates.render_upstream_oauth2_suggest_link(&ctx)?).into_response()
        }

        (None, Some(user_id)) => {
            // Session linked, but user not logged in: do the login
            let user = repo
                .user()
                .lookup(user_id)
                .await?
                .filter(mas_data_model::User::is_valid)
                .ok_or(RouteError::UserNotFound)?;

            let session = repo
                .browser_session()
                .add(&mut rng, &clock, &user, user_agent)
                .await?;

            let upstream_session = repo
                .upstream_oauth_session()
                .consume(&clock, upstream_session)
                .await?;

            repo.browser_session()
                .authenticate_with_upstream(&mut rng, &clock, &session, &upstream_session)
                .await?;

            cookie_jar = sessions_cookie
                .consume_link(link_id)?
                .save(cookie_jar, &clock);
            cookie_jar = cookie_jar.set_session(&session);

            repo.save().await?;

            post_auth_action.go_next(&url_builder).into_response()
        }

        (None, None) => {
            // Session not linked and used not logged in: suggest creating an
            // account or logging in an existing user
            let id_token = upstream_session.id_token().map(Jwt::try_from).transpose()?;

            let provider = repo
                .upstream_oauth_provider()
                .lookup(link.provider_id)
                .await?
                .ok_or(RouteError::ProviderNotFound)?;

            let ctx = UpstreamRegister::new(link.clone(), provider.clone());

            let env = environment();

            let mut context = AttributeMappingContext::new();
            if let Some(id_token) = id_token {
                let (_, payload) = id_token.into_parts();
                context = context.with_id_token_claims(payload);
            }
            if let Some(extra_callback_parameters) = upstream_session.extra_callback_parameters() {
                context = context.with_extra_callback_parameters(extra_callback_parameters.clone());
            }
            if let Some(userinfo) = upstream_session.userinfo() {
                context = context.with_userinfo_claims(userinfo.clone());
            }
            let context = context.build();

            let ctx = if provider.claims_imports.displayname.ignore() {
                ctx
            } else {
                let template = provider
                    .claims_imports
                    .displayname
                    .template
                    .as_deref()
                    .unwrap_or(DEFAULT_DISPLAYNAME_TEMPLATE);

                match render_attribute_template(
                    &env,
                    template,
                    &context,
                    provider.claims_imports.displayname.is_required(),
                )? {
                    Some(value) => ctx
                        .with_display_name(value, provider.claims_imports.displayname.is_forced()),
                    None => ctx,
                }
            };

            let ctx = if provider.claims_imports.email.ignore() {
                ctx
            } else {
                let template = provider
                    .claims_imports
                    .email
                    .template
                    .as_deref()
                    .unwrap_or(DEFAULT_EMAIL_TEMPLATE);

                match render_attribute_template(
                    &env,
                    template,
                    &context,
                    provider.claims_imports.email.is_required(),
                )? {
                    Some(value) => ctx.with_email(value, provider.claims_imports.email.is_forced()),
                    None => ctx,
                }
            };

            let ctx = if provider.claims_imports.localpart.ignore() {
                ctx
            } else {
                let template = provider
                    .claims_imports
                    .localpart
                    .template
                    .as_deref()
                    .unwrap_or(DEFAULT_LOCALPART_TEMPLATE);

                match render_attribute_template(
                    &env,
                    template,
                    &context,
                    provider.claims_imports.localpart.is_required(),
                )? {
                    Some(localpart) => {
                        // We could run policy & existing user checks when the user submits the
                        // form, but this lead to poor UX. This is why we do
                        // it ahead of time here.
                        let maybe_existing_user = repo.user().find_by_username(&localpart).await?;
                        let is_available = homeserver
                            .is_localpart_available(&localpart)
                            .await
                            .map_err(RouteError::HomeserverConnection)?;

                        if maybe_existing_user.is_some() || !is_available {
                            if let Some(existing_user) = maybe_existing_user {
                                // The mapper returned a username which already exists, but isn't
                                // linked to this upstream user.
                                warn!(username = %localpart, user_id = %existing_user.id, "Localpart template returned an existing username");
                            }

                            // TODO: translate
                            let ctx = ErrorContext::new()
                                .with_code("User exists")
                                .with_description(format!(
                                    r"Upstream account provider returned {localpart:?} as username,
                                    which is not linked to that upstream account"
                                ))
                                .with_language(&locale);

                            return Ok((
                                cookie_jar,
                                Html(templates.render_error(&ctx)?).into_response(),
                            ));
                        }

                        let res = policy
                            .evaluate_register(mas_policy::RegisterInput {
                                registration_method: mas_policy::RegistrationMethod::UpstreamOAuth2,
                                username: &localpart,
                                email: None,
                                requester: mas_policy::Requester {
                                    ip_address: activity_tracker.ip(),
                                    user_agent: user_agent.clone().map(|ua| ua.raw),
                                },
                            })
                            .await?;

                        if res.valid() {
                            // The username passes the policy check, add it to the context
                            ctx.with_localpart(
                                localpart,
                                provider.claims_imports.localpart.is_forced(),
                            )
                        } else if provider.claims_imports.localpart.is_forced() {
                            // If the username claim is 'forced' but doesn't pass the policy check,
                            // we display an error message.
                            // TODO: translate
                            let ctx = ErrorContext::new()
                                .with_code("Policy error")
                                .with_description(format!(
                                    r"Upstream account provider returned {localpart:?} as username,
                                    which does not pass the policy check: {res}"
                                ))
                                .with_language(&locale);

                            return Ok((
                                cookie_jar,
                                Html(templates.render_error(&ctx)?).into_response(),
                            ));
                        } else {
                            // Else, we just ignore it when it doesn't pass the policy check.
                            ctx
                        }
                    }
                    None => ctx,
                }
            };

            let ctx = ctx.with_csrf(csrf_token.form_value()).with_language(locale);

            Html(templates.render_upstream_oauth2_do_register(&ctx)?).into_response()
        }
    };

    Ok((cookie_jar, response))
}

#[tracing::instrument(
    name = "handlers.upstream_oauth2.link.post",
    fields(upstream_oauth_link.id = %link_id),
    skip_all,
    err,
)]
pub(crate) async fn post(
    mut rng: BoxRng,
    clock: BoxClock,
    mut repo: BoxRepository,
    cookie_jar: CookieJar,
    user_agent: Option<TypedHeader<headers::UserAgent>>,
    mut policy: Policy,
    PreferredLanguage(locale): PreferredLanguage,
    activity_tracker: BoundActivityTracker,
    State(templates): State<Templates>,
    State(homeserver): State<Arc<dyn HomeserverConnection>>,
    State(url_builder): State<UrlBuilder>,
    State(site_config): State<SiteConfig>,
    Path(link_id): Path<Ulid>,
    Form(form): Form<ProtectedForm<FormData>>,
) -> Result<Response, RouteError> {
    let user_agent = user_agent.map(|ua| UserAgent::parse(ua.as_str().to_owned()));
    let form = cookie_jar.verify_form(&clock, form)?;

    let sessions_cookie = UpstreamSessionsCookie::load(&cookie_jar);
    let (session_id, post_auth_action) = sessions_cookie
        .lookup_link(link_id)
        .map_err(|_| RouteError::MissingCookie)?;

    let post_auth_action = OptionalPostAuthAction {
        post_auth_action: post_auth_action.cloned(),
    };

    let link = repo
        .upstream_oauth_link()
        .lookup(link_id)
        .await?
        .ok_or(RouteError::LinkNotFound)?;

    let upstream_session = repo
        .upstream_oauth_session()
        .lookup(session_id)
        .await?
        .ok_or(RouteError::SessionNotFound)?;

    // This checks that we're in a browser session which is allowed to consume this
    // link: the upstream auth session should have been started in this browser.
    if upstream_session.link_id() != Some(link.id) {
        return Err(RouteError::SessionNotFound);
    }

    if upstream_session.is_consumed() {
        return Err(RouteError::SessionConsumed);
    }

    let (csrf_token, cookie_jar) = cookie_jar.csrf_token(&clock, &mut rng);
    let (user_session_info, cookie_jar) = cookie_jar.session_info();
    let maybe_user_session = user_session_info.load_session(&mut repo).await?;
    let form_state = form.to_form_state();

    let session = match (maybe_user_session, link.user_id, form) {
        (Some(session), None, FormData::Link) => {
            // The user is already logged in, the link is not linked to any user, and the
            // user asked to link their account.
            repo.upstream_oauth_link()
                .associate_to_user(&link, &session.user)
                .await?;

            session
        }

        (
            None,
            None,
            FormData::Register {
                username,
                import_email,
                import_display_name,
                accept_terms,
            },
        ) => {
            // The user got the form to register a new account, and is not logged in.
            // Depending on the claims_imports, we've let the user choose their username,
            // choose whether they want to import the email and display name, or
            // not.

            // Those fields are Some("on") if the checkbox is checked
            let import_email = import_email.is_some();
            let import_display_name = import_display_name.is_some();
            let accept_terms = accept_terms.is_some();

            let id_token = upstream_session.id_token().map(Jwt::try_from).transpose()?;

            let provider = repo
                .upstream_oauth_provider()
                .lookup(link.provider_id)
                .await?
                .ok_or(RouteError::ProviderNotFound)?;

            // Let's try to import the claims from the ID token
            let env = environment();

            let mut context = AttributeMappingContext::new();
            if let Some(id_token) = id_token {
                let (_, payload) = id_token.into_parts();
                context = context.with_id_token_claims(payload);
            }
            if let Some(extra_callback_parameters) = upstream_session.extra_callback_parameters() {
                context = context.with_extra_callback_parameters(extra_callback_parameters.clone());
            }
            if let Some(userinfo) = upstream_session.userinfo() {
                context = context.with_userinfo_claims(userinfo.clone());
            }
            let context = context.build();

            // Create a template context in case we need to re-render because of an error
            let ctx = UpstreamRegister::new(link.clone(), provider.clone());

            let display_name = if provider
                .claims_imports
                .displayname
                .should_import(import_display_name)
            {
                let template = provider
                    .claims_imports
                    .displayname
                    .template
                    .as_deref()
                    .unwrap_or(DEFAULT_DISPLAYNAME_TEMPLATE);

                render_attribute_template(
                    &env,
                    template,
                    &context,
                    provider.claims_imports.displayname.is_required(),
                )?
            } else {
                None
            };

            let ctx = if let Some(ref display_name) = display_name {
                ctx.with_display_name(
                    display_name.clone(),
                    provider.claims_imports.email.is_forced(),
                )
            } else {
                ctx
            };

            let email = if provider.claims_imports.email.should_import(import_email) {
                let template = provider
                    .claims_imports
                    .email
                    .template
                    .as_deref()
                    .unwrap_or(DEFAULT_EMAIL_TEMPLATE);

                render_attribute_template(
                    &env,
                    template,
                    &context,
                    provider.claims_imports.email.is_required(),
                )?
            } else {
                None
            };

            let ctx = if let Some(ref email) = email {
                ctx.with_email(email.clone(), provider.claims_imports.email.is_forced())
            } else {
                ctx
            };

            let username = if provider.claims_imports.localpart.is_forced() {
                let template = provider
                    .claims_imports
                    .localpart
                    .template
                    .as_deref()
                    .unwrap_or(DEFAULT_LOCALPART_TEMPLATE);

                render_attribute_template(&env, template, &context, true)?
            } else {
                // If there is no forced username, we can use the one the user entered
                username
            }
            .unwrap_or_default();

            let ctx = ctx.with_localpart(
                username.clone(),
                provider.claims_imports.localpart.is_forced(),
            );

            // Validate the form
            let form_state = {
                let mut form_state = form_state;
                let mut homeserver_denied_username = false;
                if username.is_empty() {
                    form_state.add_error_on_field(
                        mas_templates::UpstreamRegisterFormField::Username,
                        FieldError::Required,
                    );
                } else if repo.user().exists(&username).await? {
                    form_state.add_error_on_field(
                        mas_templates::UpstreamRegisterFormField::Username,
                        FieldError::Exists,
                    );
                } else if !homeserver
                    .is_localpart_available(&username)
                    .await
                    .map_err(RouteError::HomeserverConnection)?
                {
                    // The user already exists on the homeserver
                    tracing::warn!(
                        %username,
                        "Homeserver denied username provided by user"
                    );

                    // We defer adding the error on the field, until we know whether we had another
                    // error from the policy, to avoid showing both
                    homeserver_denied_username = true;
                }

                // If we need have a TOS in the config, make sure the user has accepted it
                if site_config.tos_uri.is_some() && !accept_terms {
                    form_state.add_error_on_field(
                        mas_templates::UpstreamRegisterFormField::AcceptTerms,
                        FieldError::Required,
                    );
                }

                // Policy check
                let res = policy
                    .evaluate_register(mas_policy::RegisterInput {
                        registration_method: mas_policy::RegistrationMethod::UpstreamOAuth2,
                        username: &username,
                        email: email.as_deref(),
                        requester: mas_policy::Requester {
                            ip_address: activity_tracker.ip(),
                            user_agent: user_agent.clone().map(|ua| ua.raw),
                        },
                    })
                    .await?;

                for violation in res.violations {
                    match violation.field.as_deref() {
                        Some("username") => {
                            // If the homeserver denied the username, but we also had an error on
                            // the policy side, we don't want to show
                            // both, so we reset the state here
                            homeserver_denied_username = false;
                            form_state.add_error_on_field(
                                mas_templates::UpstreamRegisterFormField::Username,
                                FieldError::Policy {
                                    code: violation.code.map(|c| c.as_str()),
                                    message: violation.msg,
                                },
                            );
                        }
                        _ => form_state.add_error_on_form(FormError::Policy {
                            code: violation.code.map(|c| c.as_str()),
                            message: violation.msg,
                        }),
                    }
                }

                if homeserver_denied_username {
                    // XXX: we may want to return different errors like "this username is reserved"
                    form_state.add_error_on_field(
                        mas_templates::UpstreamRegisterFormField::Username,
                        FieldError::Exists,
                    );
                }

                form_state
            };

            if !form_state.is_valid() {
                let ctx = ctx
                    .with_form_state(form_state)
                    .with_csrf(csrf_token.form_value())
                    .with_language(locale);

                return Ok((
                    cookie_jar,
                    Html(templates.render_upstream_oauth2_do_register(&ctx)?),
                )
                    .into_response());
            }

            // Now we can create the user
            let user = repo.user().add(&mut rng, &clock, username).await?;

            if let Some(terms_url) = &site_config.tos_uri {
                repo.user_terms()
                    .accept_terms(&mut rng, &clock, &user, terms_url.clone())
                    .await?;
            }

            // And schedule the job to provision it
            let mut job = ProvisionUserJob::new(&user);

            // If we have a display name, set it during provisioning
            if let Some(name) = display_name {
                job = job.set_display_name(name);
            }

            repo.queue_job().schedule_job(&mut rng, &clock, job).await?;

            // If we have an email, add it to the user
            if let Some(email) = email {
                repo.user_email()
                    .add(&mut rng, &clock, &user, email)
                    .await?;
            }

            repo.upstream_oauth_link()
                .associate_to_user(&link, &user)
                .await?;

            repo.browser_session()
                .add(&mut rng, &clock, &user, user_agent)
                .await?
        }

        _ => return Err(RouteError::InvalidFormAction),
    };

    let upstream_session = repo
        .upstream_oauth_session()
        .consume(&clock, upstream_session)
        .await?;

    repo.browser_session()
        .authenticate_with_upstream(&mut rng, &clock, &session, &upstream_session)
        .await?;

    let cookie_jar = sessions_cookie
        .consume_link(link_id)?
        .save(cookie_jar, &clock);
    let cookie_jar = cookie_jar.set_session(&session);

    repo.save().await?;

    Ok((cookie_jar, post_auth_action.go_next(&url_builder)).into_response())
}

#[cfg(test)]
mod tests {
    use hyper::{Request, StatusCode, header::CONTENT_TYPE};
    use mas_data_model::{
        UpstreamOAuthProviderClaimsImports, UpstreamOAuthProviderImportPreference,
        UpstreamOAuthProviderTokenAuthMethod,
    };
    use mas_iana::jose::JsonWebSignatureAlg;
    use mas_jose::jwt::{JsonWebSignatureHeader, Jwt};
    use mas_router::Route;
    use mas_storage::{
        Pagination, upstream_oauth2::UpstreamOAuthProviderParams, user::UserEmailFilter,
    };
    use oauth2_types::scope::{OPENID, Scope};
    use sqlx::PgPool;

    use super::UpstreamSessionsCookie;
    use crate::test_utils::{CookieHelper, RequestBuilderExt, ResponseExt, TestState, setup};

    #[sqlx::test(migrator = "mas_storage_pg::MIGRATOR")]
    async fn test_register(pool: PgPool) {
        setup();
        let state = TestState::from_pool(pool).await.unwrap();
        let mut rng = state.rng();
        let cookies = CookieHelper::new();

        let claims_imports = UpstreamOAuthProviderClaimsImports {
            localpart: UpstreamOAuthProviderImportPreference {
                action: mas_data_model::UpstreamOAuthProviderImportAction::Force,
                template: None,
            },
            email: UpstreamOAuthProviderImportPreference {
                action: mas_data_model::UpstreamOAuthProviderImportAction::Force,
                template: None,
            },
            ..UpstreamOAuthProviderClaimsImports::default()
        };

        let id_token = serde_json::json!({
            "preferred_username": "john",
            "email": "john@example.com",
            "email_verified": true,
        });

        // Grab a key to sign the id_token
        // We could generate a key on the fly, but because we have one available here,
        // why not use it?
        let key = state
            .key_store
            .signing_key_for_algorithm(&JsonWebSignatureAlg::Rs256)
            .unwrap();

        let signer = key
            .params()
            .signing_key_for_alg(&JsonWebSignatureAlg::Rs256)
            .unwrap();
        let header = JsonWebSignatureHeader::new(JsonWebSignatureAlg::Rs256);
        let id_token = Jwt::sign_with_rng(&mut rng, header, id_token, &signer).unwrap();

        // Provision a provider and a link
        let mut repo = state.repository().await.unwrap();
        let provider = repo
            .upstream_oauth_provider()
            .add(
                &mut rng,
                &state.clock,
                UpstreamOAuthProviderParams {
                    issuer: Some("https://example.com/".to_owned()),
                    human_name: Some("Example Ltd.".to_owned()),
                    brand_name: None,
                    scope: Scope::from_iter([OPENID]),
                    token_endpoint_auth_method: UpstreamOAuthProviderTokenAuthMethod::None,
                    token_endpoint_signing_alg: None,
                    id_token_signed_response_alg: JsonWebSignatureAlg::Rs256,
                    client_id: "client".to_owned(),
                    encrypted_client_secret: None,
                    claims_imports,
                    authorization_endpoint_override: None,
                    token_endpoint_override: None,
                    userinfo_endpoint_override: None,
                    fetch_userinfo: false,
                    userinfo_signed_response_alg: None,
                    jwks_uri_override: None,
                    discovery_mode: mas_data_model::UpstreamOAuthProviderDiscoveryMode::Oidc,
                    pkce_mode: mas_data_model::UpstreamOAuthProviderPkceMode::Auto,
                    response_mode: None,
                    additional_authorization_parameters: Vec::new(),
                },
            )
            .await
            .unwrap();

        let session = repo
            .upstream_oauth_session()
            .add(
                &mut rng,
                &state.clock,
                &provider,
                "state".to_owned(),
                None,
                "nonce".to_owned(),
            )
            .await
            .unwrap();

        let link = repo
            .upstream_oauth_link()
            .add(
                &mut rng,
                &state.clock,
                &provider,
                "subject".to_owned(),
                None,
            )
            .await
            .unwrap();

        let session = repo
            .upstream_oauth_session()
            .complete_with_link(
                &state.clock,
                session,
                &link,
                Some(id_token.into_string()),
                None,
                None,
            )
            .await
            .unwrap();

        repo.save().await.unwrap();

        let cookie_jar = state.cookie_jar();
        let upstream_sessions = UpstreamSessionsCookie::default()
            .add(session.id, provider.id, "state".to_owned(), None)
            .add_link_to_session(session.id, link.id)
            .unwrap();
        let cookie_jar = upstream_sessions.save(cookie_jar, &state.clock);
        cookies.import(cookie_jar);

        let request = Request::get(&*mas_router::UpstreamOAuth2Link::new(link.id).path()).empty();
        let request = cookies.with_cookies(request);
        let response = state.request(request).await;
        cookies.save_cookies(&response);
        response.assert_status(StatusCode::OK);
        response.assert_header_value(CONTENT_TYPE, "text/html; charset=utf-8");

        // Extract the CSRF token from the response body
        let csrf_token = response
            .body()
            .split("name=\"csrf\" value=\"")
            .nth(1)
            .unwrap()
            .split('\"')
            .next()
            .unwrap();

        let request = Request::post(&*mas_router::UpstreamOAuth2Link::new(link.id).path()).form(
            serde_json::json!({
                "csrf": csrf_token,
                "action": "register",
                "import_email": "on",
                "accept_terms": "on",
            }),
        );
        let request = cookies.with_cookies(request);
        let response = state.request(request).await;
        cookies.save_cookies(&response);
        response.assert_status(StatusCode::SEE_OTHER);

        // Check that we have a registered user, with the email imported
        let mut repo = state.repository().await.unwrap();
        let user = repo
            .user()
            .find_by_username("john")
            .await
            .unwrap()
            .expect("user exists");

        let link = repo
            .upstream_oauth_link()
            .find_by_subject(&provider, "subject")
            .await
            .unwrap()
            .expect("link exists");

        assert_eq!(link.user_id, Some(user.id));

        let page = repo
            .user_email()
            .list(UserEmailFilter::new().for_user(&user), Pagination::first(1))
            .await
            .unwrap();
        let email = page.edges.first().expect("email exists");

        assert_eq!(email.email, "john@example.com");
    }
}
