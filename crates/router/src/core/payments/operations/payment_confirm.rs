use std::marker::PhantomData;

use api_models::{
    admin::ExtendedCardInfoConfig,
    enums::FrmSuggestion,
    payment_methods::PaymentMethodsData,
    payments::{AdditionalPaymentData, ExtendedCardInfo},
};
use async_trait::async_trait;
use common_utils::ext_traits::{AsyncExt, Encode, StringExt, ValueExt};
use error_stack::{report, ResultExt};
use futures::FutureExt;
use masking::{ExposeInterface, PeekInterface};
use router_derive::PaymentOperation;
use router_env::{instrument, logger, tracing};
use tracing_futures::Instrument;

use super::{BoxedOperation, Domain, GetTracker, Operation, UpdateTracker, ValidateRequest};
use crate::{
    core::{
        authentication,
        blocklist::utils as blocklist_utils,
        errors::{self, CustomResult, RouterResult, StorageErrorExt},
        mandate::helpers as m_helpers,
        payments::{
            self, helpers, operations, populate_surcharge_details, CustomerDetails, PaymentAddress,
            PaymentData,
        },
        utils as core_utils,
    },
    db::StorageInterface,
    events::audit_events::{AuditEvent, AuditEventType},
    routes::{app::ReqState, SessionState},
    services,
    types::{
        self,
        api::{self, ConnectorCallType, PaymentIdTypeExt},
        domain::{self, types::decrypt},
        storage::{self, enums as storage_enums},
    },
    utils::{self, OptionExt},
};

#[derive(Debug, Clone, Copy, PaymentOperation)]
#[operation(operations = "all", flow = "authorize")]
pub struct PaymentConfirm;
#[async_trait]
impl<F: Send + Clone> GetTracker<F, PaymentData<F>, api::PaymentsRequest> for PaymentConfirm {
    #[instrument(skip_all)]
    async fn get_trackers<'a>(
        &'a self,
        state: &'a SessionState,
        payment_id: &api::PaymentIdType,
        request: &api::PaymentsRequest,
        merchant_account: &domain::MerchantAccount,
        key_store: &domain::MerchantKeyStore,
        auth_flow: services::AuthFlow,
        payment_confirm_source: Option<common_enums::PaymentSource>,
    ) -> RouterResult<operations::GetTrackerResponse<'a, F, api::PaymentsRequest>> {
        let merchant_id = &merchant_account.merchant_id;
        let storage_scheme = merchant_account.storage_scheme;
        let (currency, amount);

        let payment_id = payment_id
            .get_payment_intent_id()
            .change_context(errors::ApiErrorResponse::PaymentNotFound)?;

        // Stage 1
        let store = &*state.store;
        let m_merchant_id = merchant_id.clone();

        // Parallel calls - level 0
        let mut payment_intent = store
            .find_payment_intent_by_payment_id_merchant_id(
                &payment_id,
                m_merchant_id.as_str(),
                key_store,
                storage_scheme,
            )
            .await
            .to_not_found_response(errors::ApiErrorResponse::PaymentNotFound)?;

        if let Some(order_details) = &request.order_details {
            helpers::validate_order_details_amount(
                order_details.to_owned(),
                payment_intent.amount.get_amount_as_i64(),
                false,
            )?;
        }

        helpers::validate_customer_access(&payment_intent, auth_flow, request)?;

        if [
            Some(common_enums::PaymentSource::Webhook),
            Some(common_enums::PaymentSource::ExternalAuthenticator),
        ]
        .contains(&payment_confirm_source)
        {
            helpers::validate_payment_status_against_not_allowed_statuses(
                &payment_intent.status,
                &[
                    storage_enums::IntentStatus::Cancelled,
                    storage_enums::IntentStatus::Succeeded,
                    storage_enums::IntentStatus::Processing,
                    storage_enums::IntentStatus::RequiresCapture,
                    storage_enums::IntentStatus::RequiresMerchantAction,
                ],
                "confirm",
            )?;
        } else {
            helpers::validate_payment_status_against_not_allowed_statuses(
                &payment_intent.status,
                &[
                    storage_enums::IntentStatus::Cancelled,
                    storage_enums::IntentStatus::Succeeded,
                    storage_enums::IntentStatus::Processing,
                    storage_enums::IntentStatus::RequiresCapture,
                    storage_enums::IntentStatus::RequiresMerchantAction,
                    storage_enums::IntentStatus::RequiresCustomerAction,
                ],
                "confirm",
            )?;
        }

        helpers::authenticate_client_secret(request.client_secret.as_ref(), &payment_intent)?;

        let customer_details = helpers::get_customer_details_from_request(request);

        // Stage 2
        let attempt_id = payment_intent.active_attempt.get_id();
        let profile_id = payment_intent
            .profile_id
            .clone()
            .get_required_value("profile_id")
            .change_context(errors::ApiErrorResponse::InternalServerError)
            .attach_printable("'profile_id' not set in payment intent")?;

        let store = state.store.clone();

        let business_profile_fut = tokio::spawn(
            async move {
                store
                    .find_business_profile_by_profile_id(&profile_id)
                    .map(|business_profile_result| {
                        business_profile_result.to_not_found_response(
                            errors::ApiErrorResponse::BusinessProfileNotFound {
                                id: profile_id.to_string(),
                            },
                        )
                    })
                    .await
            }
            .in_current_span(),
        );

        let store = state.store.clone();

        let m_payment_id = payment_intent.payment_id.clone();
        let m_merchant_id = merchant_id.clone();

        let payment_attempt_fut = tokio::spawn(
            async move {
                store
                    .find_payment_attempt_by_payment_id_merchant_id_attempt_id(
                        m_payment_id.as_str(),
                        m_merchant_id.as_str(),
                        attempt_id.as_str(),
                        storage_scheme,
                    )
                    .map(|x| x.to_not_found_response(errors::ApiErrorResponse::PaymentNotFound))
                    .await
            }
            .in_current_span(),
        );

        let m_merchant_id = merchant_id.clone();
        let m_request_shipping = request.shipping.clone();
        let m_payment_intent_shipping_address_id = payment_intent.shipping_address_id.clone();
        let m_payment_intent_payment_id = payment_intent.payment_id.clone();
        let m_customer_details_customer_id = customer_details.customer_id.clone();
        let m_payment_intent_customer_id = payment_intent.customer_id.clone();
        let store = state.clone().store;
        let m_key_store = key_store.clone();

        let shipping_address_fut = tokio::spawn(
            async move {
                helpers::create_or_update_address_for_payment_by_request(
                    store.as_ref(),
                    m_request_shipping.as_ref(),
                    m_payment_intent_shipping_address_id.as_deref(),
                    m_merchant_id.as_str(),
                    m_payment_intent_customer_id
                        .as_ref()
                        .or(m_customer_details_customer_id.as_ref()),
                    &m_key_store,
                    m_payment_intent_payment_id.as_ref(),
                    storage_scheme,
                )
                .await
            }
            .in_current_span(),
        );

        let m_merchant_id = merchant_id.clone();
        let m_request_billing = request.billing.clone();
        let m_customer_details_customer_id = customer_details.customer_id.clone();
        let m_payment_intent_customer_id = payment_intent.customer_id.clone();
        let m_payment_intent_billing_address_id = payment_intent.billing_address_id.clone();
        let m_payment_intent_payment_id = payment_intent.payment_id.clone();
        let store = state.clone().store;
        let m_key_store = key_store.clone();

        let billing_address_fut = tokio::spawn(
            async move {
                helpers::create_or_update_address_for_payment_by_request(
                    store.as_ref(),
                    m_request_billing.as_ref(),
                    m_payment_intent_billing_address_id.as_deref(),
                    m_merchant_id.as_ref(),
                    m_payment_intent_customer_id
                        .as_ref()
                        .or(m_customer_details_customer_id.as_ref()),
                    &m_key_store,
                    m_payment_intent_payment_id.as_ref(),
                    storage_scheme,
                )
                .await
            }
            .in_current_span(),
        );

        let m_merchant_id = merchant_id.clone();
        let store = state.clone().store;
        let m_request_merchant_connector_details = request.merchant_connector_details.clone();

        let config_update_fut = tokio::spawn(
            async move {
                m_request_merchant_connector_details
                    .async_map(|mcd| async {
                        helpers::insert_merchant_connector_creds_to_config(
                            store.as_ref(),
                            m_merchant_id.as_str(),
                            mcd,
                        )
                        .await
                    })
                    .map(|x| x.transpose())
                    .await
            }
            .in_current_span(),
        );

        // Based on whether a retry can be performed or not, fetch relevant entities
        let (mut payment_attempt, shipping_address, billing_address, business_profile) =
            match payment_intent.status {
                api_models::enums::IntentStatus::RequiresCustomerAction
                | api_models::enums::IntentStatus::RequiresMerchantAction
                | api_models::enums::IntentStatus::RequiresPaymentMethod
                | api_models::enums::IntentStatus::RequiresConfirmation => {
                    // Normal payment
                    // Parallel calls - level 1
                    let (payment_attempt, shipping_address, billing_address, business_profile, _) =
                        tokio::try_join!(
                            utils::flatten_join_error(payment_attempt_fut),
                            utils::flatten_join_error(shipping_address_fut),
                            utils::flatten_join_error(billing_address_fut),
                            utils::flatten_join_error(business_profile_fut),
                            utils::flatten_join_error(config_update_fut)
                        )?;

                    (
                        payment_attempt,
                        shipping_address,
                        billing_address,
                        business_profile,
                    )
                }
                _ => {
                    // Retry payment
                    let (
                        mut payment_attempt,
                        shipping_address,
                        billing_address,
                        business_profile,
                        _,
                    ) = tokio::try_join!(
                        utils::flatten_join_error(payment_attempt_fut),
                        utils::flatten_join_error(shipping_address_fut),
                        utils::flatten_join_error(billing_address_fut),
                        utils::flatten_join_error(business_profile_fut),
                        utils::flatten_join_error(config_update_fut)
                    )?;

                    let attempt_type = helpers::get_attempt_type(
                        &payment_intent,
                        &payment_attempt,
                        request,
                        "confirm",
                    )?;

                    // 3
                    (payment_intent, payment_attempt) = attempt_type
                        .modify_payment_intent_and_payment_attempt(
                            request,
                            payment_intent,
                            payment_attempt,
                            &*state.store,
                            key_store,
                            storage_scheme,
                        )
                        .await?;

                    (
                        payment_attempt,
                        shipping_address,
                        billing_address,
                        business_profile,
                    )
                }
            };

        payment_intent.order_details = request
            .get_order_details_as_value()
            .change_context(errors::ApiErrorResponse::InternalServerError)
            .attach_printable("Failed to convert order details to value")?
            .or(payment_intent.order_details);

        payment_intent.setup_future_usage = request
            .setup_future_usage
            .or(payment_intent.setup_future_usage);

        let browser_info = request
            .browser_info
            .clone()
            .or(payment_attempt.browser_info)
            .as_ref()
            .map(Encode::encode_to_value)
            .transpose()
            .change_context(errors::ApiErrorResponse::InvalidDataValue {
                field_name: "browser_info",
            })?;
        let customer_acceptance = request.customer_acceptance.clone().map(From::from);

        let recurring_details = request.recurring_details.clone();

        helpers::validate_card_data(
            request
                .payment_method_data
                .as_ref()
                .and_then(|pmd| pmd.payment_method_data.clone()),
        )?;

        payment_attempt.browser_info = browser_info;

        payment_attempt.payment_experience = request
            .payment_experience
            .or(payment_attempt.payment_experience);

        payment_attempt.capture_method = request.capture_method.or(payment_attempt.capture_method);

        currency = payment_attempt.currency.get_required_value("currency")?;
        amount = payment_attempt.get_total_amount().into();

        helpers::validate_customer_id_mandatory_cases(
            request.setup_future_usage.is_some(),
            payment_intent
                .customer_id
                .as_ref()
                .or(customer_details.customer_id.as_ref()),
        )?;

        let creds_identifier = request
            .merchant_connector_details
            .as_ref()
            .map(|mcd| mcd.creds_identifier.to_owned());

        payment_intent.shipping_address_id =
            shipping_address.as_ref().map(|i| i.address_id.clone());
        payment_intent.billing_address_id = billing_address.as_ref().map(|i| i.address_id.clone());
        payment_intent.return_url = request
            .return_url
            .as_ref()
            .map(|a| a.to_string())
            .or(payment_intent.return_url);

        payment_intent.allowed_payment_method_types = request
            .get_allowed_payment_method_types_as_value()
            .change_context(errors::ApiErrorResponse::InternalServerError)
            .attach_printable("Error converting allowed_payment_types to Value")?
            .or(payment_intent.allowed_payment_method_types);

        payment_intent.connector_metadata = request
            .get_connector_metadata_as_value()
            .change_context(errors::ApiErrorResponse::InternalServerError)
            .attach_printable("Error converting connector_metadata to Value")?
            .or(payment_intent.connector_metadata);

        payment_intent.feature_metadata = request
            .get_feature_metadata_as_value()
            .change_context(errors::ApiErrorResponse::InternalServerError)
            .attach_printable("Error converting feature_metadata to Value")?
            .or(payment_intent.feature_metadata);
        payment_intent.metadata = request.metadata.clone().or(payment_intent.metadata);
        payment_intent.frm_metadata = request.frm_metadata.clone().or(payment_intent.frm_metadata);
        payment_intent.request_incremental_authorization = request
            .request_incremental_authorization
            .map(|request_incremental_authorization| {
                core_utils::get_request_incremental_authorization_value(
                    Some(request_incremental_authorization),
                    payment_attempt.capture_method,
                )
            })
            .unwrap_or(Ok(payment_intent.request_incremental_authorization))?;
        payment_attempt.business_sub_label = request
            .business_sub_label
            .clone()
            .or(payment_attempt.business_sub_label);

        let n_request_payment_method_data = request
            .payment_method_data
            .as_ref()
            .and_then(|pmd| pmd.payment_method_data.clone());

        let store = state.clone().store;
        let profile_id = payment_intent
            .profile_id
            .clone()
            .get_required_value("profile_id")
            .change_context(errors::ApiErrorResponse::InternalServerError)
            .attach_printable("'profile_id' not set in payment intent")?;

        let additional_pm_data_fut = tokio::spawn(
            async move {
                Ok(n_request_payment_method_data
                    .async_map(|payment_method_data| async move {
                        helpers::get_additional_payment_data(
                            &payment_method_data,
                            store.as_ref(),
                            profile_id.as_ref(),
                        )
                        .await
                    })
                    .await)
            }
            .in_current_span(),
        );

        let store = state.clone().store;

        let n_payment_method_billing_address_id =
            payment_attempt.payment_method_billing_address_id.clone();
        let n_request_payment_method_billing_address = request
            .payment_method_data
            .as_ref()
            .and_then(|pmd| pmd.billing.clone());
        let m_payment_intent_customer_id = payment_intent.customer_id.clone();
        let m_payment_intent_payment_id = payment_intent.payment_id.clone();
        let m_key_store = key_store.clone();
        let m_customer_details_customer_id = customer_details.customer_id.clone();
        let m_merchant_id = merchant_id.clone();

        let payment_method_billing_future = tokio::spawn(
            async move {
                helpers::create_or_update_address_for_payment_by_request(
                    store.as_ref(),
                    n_request_payment_method_billing_address.as_ref(),
                    n_payment_method_billing_address_id.as_deref(),
                    m_merchant_id.as_str(),
                    m_payment_intent_customer_id
                        .as_ref()
                        .or(m_customer_details_customer_id.as_ref()),
                    &m_key_store,
                    m_payment_intent_payment_id.as_ref(),
                    storage_scheme,
                )
                .await
            }
            .in_current_span(),
        );

        let mandate_type = m_helpers::get_mandate_type(
            request.mandate_data.clone(),
            request.off_session,
            payment_intent.setup_future_usage,
            request.customer_acceptance.clone(),
            request.payment_token.clone(),
        )
        .change_context(errors::ApiErrorResponse::MandateValidationFailed {
            reason: "Expected one out of recurring_details and mandate_data but got both".into(),
        })?;

        let m_state = state.clone();
        let m_mandate_type = mandate_type.clone();
        let m_merchant_account = merchant_account.clone();
        let m_request = request.clone();
        let m_key_store = key_store.clone();

        let payment_intent_customer_id = payment_intent.customer_id.clone();

        let mandate_details_fut = tokio::spawn(
            async move {
                helpers::get_token_pm_type_mandate_details(
                    &m_state,
                    &m_request,
                    m_mandate_type,
                    &m_merchant_account,
                    &m_key_store,
                    None,
                    &payment_intent_customer_id,
                )
                .await
            }
            .in_current_span(),
        );

        // Parallel calls - level 2
        let (mandate_details, additional_pm_data, payment_method_billing) = tokio::try_join!(
            utils::flatten_join_error(mandate_details_fut),
            utils::flatten_join_error(additional_pm_data_fut),
            utils::flatten_join_error(payment_method_billing_future),
        )?;

        let m_helpers::MandateGenericData {
            token,
            payment_method,
            payment_method_type,
            mandate_data,
            recurring_mandate_payment_data,
            mandate_connector,
            payment_method_info,
        } = mandate_details;

        payment_attempt.payment_method = payment_method.or(payment_attempt.payment_method);

        payment_attempt.payment_method_type = payment_method_type
            .or(payment_attempt.payment_method_type)
            .or(payment_method_info
                .as_ref()
                .and_then(|pm_info| pm_info.payment_method_type));

        let token = token.or_else(|| payment_attempt.payment_token.clone());

        helpers::validate_pm_or_token_given(
            &request.payment_method,
            &request
                .payment_method_data
                .as_ref()
                .and_then(|pmd| pmd.payment_method_data.clone()),
            &request.payment_method_type,
            &mandate_type,
            &token,
        )?;

        let (token_data, payment_method_info) = if let Some(token) = token.clone() {
            let token_data = helpers::retrieve_payment_token_data(
                state,
                token,
                payment_method.or(payment_attempt.payment_method),
            )
            .await?;

            let payment_method_info = helpers::retrieve_payment_method_from_db_with_token_data(
                state,
                &token_data,
                storage_scheme,
            )
            .await?;

            (Some(token_data), payment_method_info)
        } else {
            (None, payment_method_info)
        };

        // The operation merges mandate data from both request and payment_attempt
        let setup_mandate = mandate_data.map(|mut sm| {
            sm.mandate_type = payment_attempt.mandate_details.clone().or(sm.mandate_type);
            sm.update_mandate_id = payment_attempt
                .mandate_data
                .clone()
                .and_then(|mandate| mandate.update_mandate_id)
                .or(sm.update_mandate_id);
            sm
        });

        let mandate_details_present =
            payment_attempt.mandate_details.is_some() || request.mandate_data.is_some();
        helpers::validate_mandate_data_and_future_usage(
            payment_intent.setup_future_usage,
            mandate_details_present,
        )?;

        let payment_method_data_after_card_bin_call = request
            .payment_method_data
            .as_ref()
            .and_then(|request_payment_method_data| {
                request_payment_method_data.payment_method_data.as_ref()
            })
            .zip(additional_pm_data)
            .map(|(payment_method_data, additional_payment_data)| {
                payment_method_data.apply_additional_payment_data(additional_payment_data)
            });

        payment_attempt.payment_method_billing_address_id = payment_method_billing
            .as_ref()
            .map(|payment_method_billing| payment_method_billing.address_id.clone());

        let payment_data = PaymentData {
            flow: PhantomData,
            payment_intent,
            payment_attempt,
            currency,
            amount,
            email: request.email.clone(),
            mandate_id: None,
            mandate_connector,
            setup_mandate,
            customer_acceptance,
            token,
            address: PaymentAddress::new(
                shipping_address.as_ref().map(From::from),
                billing_address.as_ref().map(From::from),
                payment_method_billing.as_ref().map(From::from),
                business_profile.use_billing_as_payment_method_billing,
            ),
            token_data,
            confirm: request.confirm,
            payment_method_data: payment_method_data_after_card_bin_call,
            payment_method_info,
            force_sync: None,
            refunds: vec![],
            disputes: vec![],
            attempts: None,
            sessions_token: vec![],
            card_cvc: request.card_cvc.clone(),
            creds_identifier,
            pm_token: None,
            connector_customer_id: None,
            recurring_mandate_payment_data,
            ephemeral_key: None,
            multiple_capture_data: None,
            redirect_response: None,
            surcharge_details: None,
            frm_message: None,
            payment_link_data: None,
            incremental_authorization_details: None,
            authorizations: vec![],
            authentication: None,
            recurring_details,
            poll_config: None,
        };

        let get_trackers_response = operations::GetTrackerResponse {
            operation: Box::new(self),
            customer_details: Some(customer_details),
            payment_data,
            business_profile,
            mandate_type,
        };

        Ok(get_trackers_response)
    }
}

#[async_trait]
impl<F: Clone + Send> Domain<F, api::PaymentsRequest> for PaymentConfirm {
    #[instrument(skip_all)]
    async fn get_or_create_customer_details<'a>(
        &'a self,
        db: &dyn StorageInterface,
        payment_data: &mut PaymentData<F>,
        request: Option<CustomerDetails>,
        key_store: &domain::MerchantKeyStore,
        storage_scheme: common_enums::enums::MerchantStorageScheme,
    ) -> CustomResult<
        (
            BoxedOperation<'a, F, api::PaymentsRequest>,
            Option<domain::Customer>,
        ),
        errors::StorageError,
    > {
        helpers::create_customer_if_not_exist(
            Box::new(self),
            db,
            payment_data,
            request,
            &key_store.merchant_id,
            key_store,
            storage_scheme,
        )
        .await
    }

    #[instrument(skip_all)]
    async fn make_pm_data<'a>(
        &'a self,
        state: &'a SessionState,
        payment_data: &mut PaymentData<F>,
        storage_scheme: storage_enums::MerchantStorageScheme,
        key_store: &domain::MerchantKeyStore,
        customer: &Option<domain::Customer>,
        business_profile: Option<&diesel_models::business_profile::BusinessProfile>,
    ) -> RouterResult<(
        BoxedOperation<'a, F, api::PaymentsRequest>,
        Option<api::PaymentMethodData>,
        Option<String>,
    )> {
        let (op, payment_method_data, pm_id) = helpers::make_pm_data(
            Box::new(self),
            state,
            payment_data,
            key_store,
            customer,
            storage_scheme,
            business_profile,
        )
        .await?;

        utils::when(payment_method_data.is_none(), || {
            Err(errors::ApiErrorResponse::PaymentMethodNotFound)
        })?;

        Ok((op, payment_method_data, pm_id))
    }

    #[instrument(skip_all)]
    async fn add_task_to_process_tracker<'a>(
        &'a self,
        state: &'a SessionState,
        payment_attempt: &storage::PaymentAttempt,
        requeue: bool,
        schedule_time: Option<time::PrimitiveDateTime>,
    ) -> CustomResult<(), errors::ApiErrorResponse> {
        // This spawns this futures in a background thread, the exception inside this future won't affect
        // the current thread and the lifecycle of spawn thread is not handled by runtime.
        // So when server shutdown won't wait for this thread's completion.
        let m_payment_attempt = payment_attempt.clone();
        let m_state = state.clone();
        let m_self = *self;
        tokio::spawn(
            async move {
                helpers::add_domain_task_to_pt(
                    &m_self,
                    &m_state,
                    &m_payment_attempt,
                    requeue,
                    schedule_time,
                )
                .await
            }
            .in_current_span(),
        );

        Ok(())
    }

    async fn get_connector<'a>(
        &'a self,
        _merchant_account: &domain::MerchantAccount,
        state: &SessionState,
        request: &api::PaymentsRequest,
        _payment_intent: &storage::PaymentIntent,
        _key_store: &domain::MerchantKeyStore,
    ) -> CustomResult<api::ConnectorChoice, errors::ApiErrorResponse> {
        // Use a new connector in the confirm call or use the same one which was passed when
        // creating the payment or if none is passed then use the routing algorithm
        helpers::get_connector_default(state, request.routing.clone()).await
    }

    #[instrument(skip_all)]
    async fn populate_payment_data<'a>(
        &'a self,
        state: &SessionState,
        payment_data: &mut PaymentData<F>,
        _merchant_account: &domain::MerchantAccount,
    ) -> CustomResult<(), errors::ApiErrorResponse> {
        populate_surcharge_details(state, payment_data).await
    }

    async fn call_external_three_ds_authentication_if_eligible<'a>(
        &'a self,
        state: &SessionState,
        payment_data: &mut PaymentData<F>,
        should_continue_confirm_transaction: &mut bool,
        connector_call_type: &ConnectorCallType,
        business_profile: &storage::BusinessProfile,
        key_store: &domain::MerchantKeyStore,
    ) -> CustomResult<(), errors::ApiErrorResponse> {
        let external_authentication_flow =
            helpers::get_payment_external_authentication_flow_during_confirm(
                state,
                key_store,
                business_profile,
                payment_data,
                connector_call_type,
            )
            .await?;
        payment_data.authentication = match external_authentication_flow {
            Some(helpers::PaymentExternalAuthenticationFlow::PreAuthenticationFlow {
                acquirer_details,
                card_number,
                token,
            }) => {
                let authentication = authentication::perform_pre_authentication(
                    state,
                    key_store,
                    card_number,
                    token,
                    business_profile,
                    Some(acquirer_details),
                    Some(payment_data.payment_attempt.payment_id.clone()),
                )
                .await?;
                if authentication.is_separate_authn_required()
                    || authentication.authentication_status.is_failed()
                {
                    *should_continue_confirm_transaction = false;
                    let default_poll_config = types::PollConfig::default();
                    let default_config_str = default_poll_config
                        .encode_to_string_of_json()
                        .change_context(errors::ApiErrorResponse::InternalServerError)
                        .attach_printable("Error while stringifying default poll config")?;
                    let poll_config = state
                        .store
                        .find_config_by_key_unwrap_or(
                            &types::PollConfig::get_poll_config_key(
                                authentication.authentication_connector.clone(),
                            ),
                            Some(default_config_str),
                        )
                        .await
                        .change_context(errors::ApiErrorResponse::InternalServerError)
                        .attach_printable("The poll config was not found in the DB")?;
                    let poll_config: types::PollConfig = poll_config
                        .config
                        .parse_struct("PollConfig")
                        .change_context(errors::ApiErrorResponse::InternalServerError)
                        .attach_printable("Error while parsing PollConfig")?;
                    payment_data.poll_config = Some(poll_config)
                }
                Some(authentication)
            }
            Some(helpers::PaymentExternalAuthenticationFlow::PostAuthenticationFlow {
                authentication_id,
            }) => {
                let authentication = authentication::perform_post_authentication(
                    state,
                    key_store,
                    business_profile.clone(),
                    authentication_id.clone(),
                )
                .await?;
                //If authentication is not successful, skip the payment connector flows and mark the payment as failure
                if authentication.authentication_status
                    != api_models::enums::AuthenticationStatus::Success
                {
                    *should_continue_confirm_transaction = false;
                }
                Some(authentication)
            }
            None => None,
        };
        Ok(())
    }

    #[instrument(skip_all)]
    async fn guard_payment_against_blocklist<'a>(
        &'a self,
        state: &SessionState,
        merchant_account: &domain::MerchantAccount,
        key_store: &domain::MerchantKeyStore,
        payment_data: &mut PaymentData<F>,
    ) -> CustomResult<bool, errors::ApiErrorResponse> {
        blocklist_utils::validate_data_for_blocklist(
            state,
            merchant_account,
            key_store,
            payment_data,
        )
        .await
    }

    #[instrument(skip_all)]
    async fn store_extended_card_info_temporarily<'a>(
        &'a self,
        state: &SessionState,
        payment_id: &str,
        business_profile: &storage::BusinessProfile,
        payment_method_data: &Option<api::PaymentMethodData>,
    ) -> CustomResult<(), errors::ApiErrorResponse> {
        if let (Some(true), Some(api::PaymentMethodData::Card(card)), Some(merchant_config)) = (
            business_profile.is_extended_card_info_enabled,
            payment_method_data,
            business_profile.extended_card_info_config.clone(),
        ) {
            let merchant_config = merchant_config
                    .expose()
                    .parse_value::<ExtendedCardInfoConfig>("ExtendedCardInfoConfig")
                    .map_err(|err| logger::error!(parse_err=?err,"Error while parsing ExtendedCardInfoConfig"));

            let card_data = ExtendedCardInfo::from(card.clone())
                    .encode_to_vec()
                    .map_err(|err| logger::error!(encode_err=?err,"Error while encoding ExtendedCardInfo to vec"));

            let (Ok(merchant_config), Ok(card_data)) = (merchant_config, card_data) else {
                return Ok(());
            };

            let encrypted_payload =
                    services::encrypt_jwe(&card_data, merchant_config.public_key.peek())
                        .await
                        .map_err(|err| {
                            logger::error!(jwe_encryption_err=?err,"Error while JWE encrypting extended card info")
                        });

            let Ok(encrypted_payload) = encrypted_payload else {
                return Ok(());
            };

            let redis_conn = state
                .store
                .get_redis_conn()
                .change_context(errors::ApiErrorResponse::InternalServerError)
                .attach_printable("Failed to get redis connection")?;

            let key = helpers::get_redis_key_for_extended_card_info(
                &business_profile.merchant_id,
                payment_id,
            );

            redis_conn
                .set_key_with_expiry(
                    &key,
                    encrypted_payload.clone(),
                    (*merchant_config.ttl_in_secs).into(),
                )
                .await
                .change_context(errors::ApiErrorResponse::InternalServerError)
                .attach_printable("Failed to add extended card info in redis")?;

            logger::info!("Extended card info added to redis");
        }

        Ok(())
    }
}

#[async_trait]
impl<F: Clone> UpdateTracker<F, PaymentData<F>, api::PaymentsRequest> for PaymentConfirm {
    #[instrument(skip_all)]
    async fn update_trackers<'b>(
        &'b self,
        state: &'b SessionState,
        req_state: ReqState,
        mut payment_data: PaymentData<F>,
        customer: Option<domain::Customer>,
        storage_scheme: storage_enums::MerchantStorageScheme,
        updated_customer: Option<storage::CustomerUpdate>,
        key_store: &domain::MerchantKeyStore,
        frm_suggestion: Option<FrmSuggestion>,
        header_payload: api::HeaderPayload,
    ) -> RouterResult<(BoxedOperation<'b, F, api::PaymentsRequest>, PaymentData<F>)>
    where
        F: 'b + Send,
    {
        let payment_method = payment_data.payment_attempt.payment_method;
        let browser_info = payment_data.payment_attempt.browser_info.clone();
        let frm_message = payment_data.frm_message.clone();
        let capture_method = payment_data.payment_attempt.capture_method;

        let default_status_result = (
            storage_enums::IntentStatus::Processing,
            storage_enums::AttemptStatus::Pending,
            (None, None),
        );
        let status_handler_for_frm_results = |frm_suggestion: FrmSuggestion| match frm_suggestion {
            FrmSuggestion::FrmCancelTransaction => (
                storage_enums::IntentStatus::Failed,
                storage_enums::AttemptStatus::Failure,
                frm_message.map_or((None, None), |fraud_check| {
                    (
                        Some(Some(fraud_check.frm_status.to_string())),
                        Some(fraud_check.frm_reason.map(|reason| reason.to_string())),
                    )
                }),
            ),
            FrmSuggestion::FrmManualReview => (
                storage_enums::IntentStatus::RequiresMerchantAction,
                storage_enums::AttemptStatus::Unresolved,
                (None, None),
            ),
            FrmSuggestion::FrmAuthorizeTransaction => (
                storage_enums::IntentStatus::RequiresCapture,
                storage_enums::AttemptStatus::Authorized,
                (None, None),
            ),
        };

        let status_handler_for_authentication_results =
            |authentication: &storage::Authentication| {
                if authentication.authentication_status.is_failed() {
                    (
                        storage_enums::IntentStatus::Failed,
                        storage_enums::AttemptStatus::Failure,
                        (
                            Some(Some("EXTERNAL_AUTHENTICATION_FAILURE".to_string())),
                            Some(Some("external authentication failure".to_string())),
                        ),
                    )
                } else if authentication.is_separate_authn_required() {
                    (
                        storage_enums::IntentStatus::RequiresCustomerAction,
                        storage_enums::AttemptStatus::AuthenticationPending,
                        (None, None),
                    )
                } else {
                    default_status_result.clone()
                }
            };

        let (intent_status, attempt_status, (error_code, error_message)) =
            match (frm_suggestion, payment_data.authentication.as_ref()) {
                (Some(frm_suggestion), _) => status_handler_for_frm_results(frm_suggestion),
                (_, Some(authentication_details)) => {
                    status_handler_for_authentication_results(authentication_details)
                }
                _ => default_status_result,
            };

        let connector = payment_data.payment_attempt.connector.clone();
        let merchant_connector_id = payment_data.payment_attempt.merchant_connector_id.clone();

        let straight_through_algorithm = payment_data
            .payment_attempt
            .straight_through_algorithm
            .clone();
        let payment_token = payment_data.token.clone();
        let payment_method_type = payment_data.payment_attempt.payment_method_type;
        let profile_id = payment_data
            .payment_intent
            .profile_id
            .as_ref()
            .get_required_value("profile_id")
            .change_context(errors::ApiErrorResponse::InternalServerError)?;
        let payment_experience = payment_data.payment_attempt.payment_experience;
        let additional_pm_data = payment_data
            .payment_method_data
            .as_ref()
            .async_map(|payment_method_data| async {
                helpers::get_additional_payment_data(payment_method_data, &*state.store, profile_id)
                    .await
            })
            .await
            .as_ref()
            .map(Encode::encode_to_value)
            .transpose()
            .change_context(errors::ApiErrorResponse::InternalServerError)
            .attach_printable("Failed to encode additional pm data")?;
        let encode_additional_pm_to_value = if let Some(ref pm) = payment_data.payment_method_info {
            let key = key_store.key.get_inner().peek();

            let card_detail_from_locker: Option<api::CardDetailFromLocker> =
                decrypt::<serde_json::Value, masking::WithType>(
                    pm.payment_method_data.clone(),
                    key,
                )
                .await
                .change_context(errors::StorageError::DecryptionError)
                .attach_printable("unable to decrypt card details")
                .ok()
                .flatten()
                .map(|x| x.into_inner().expose())
                .and_then(|v| serde_json::from_value::<PaymentMethodsData>(v).ok())
                .and_then(|pmd| match pmd {
                    PaymentMethodsData::Card(crd) => Some(api::CardDetailFromLocker::from(crd)),
                    _ => None,
                });

            card_detail_from_locker.and_then(|card_details| {
                let additional_data = card_details.into();
                let additional_data_payment =
                    AdditionalPaymentData::Card(Box::new(additional_data));
                additional_data_payment
                    .encode_to_value()
                    .change_context(errors::ApiErrorResponse::InternalServerError)
                    .attach_printable("Failed to encode additional pm data")
                    .ok()
            })
        } else {
            None
        };

        let customer_details = payment_data.payment_intent.customer_details.clone();
        let business_sub_label = payment_data.payment_attempt.business_sub_label.clone();
        let authentication_type = payment_data.payment_attempt.authentication_type;

        let (shipping_address_id, billing_address_id, payment_method_billing_address_id) = (
            payment_data.payment_intent.shipping_address_id.clone(),
            payment_data.payment_intent.billing_address_id.clone(),
            payment_data
                .payment_attempt
                .payment_method_billing_address_id
                .clone(),
        );

        let customer_id = customer.clone().map(|c| c.customer_id);
        let return_url = payment_data.payment_intent.return_url.take();
        let setup_future_usage = payment_data.payment_intent.setup_future_usage;
        let business_label = payment_data.payment_intent.business_label.clone();
        let business_country = payment_data.payment_intent.business_country;
        let description = payment_data.payment_intent.description.take();
        let statement_descriptor_name =
            payment_data.payment_intent.statement_descriptor_name.take();
        let statement_descriptor_suffix = payment_data
            .payment_intent
            .statement_descriptor_suffix
            .take();
        let order_details = payment_data.payment_intent.order_details.clone();
        let metadata = payment_data.payment_intent.metadata.clone();
        let frm_metadata = payment_data.payment_intent.frm_metadata.clone();
        let authorized_amount = payment_data
            .surcharge_details
            .as_ref()
            .map(|surcharge_details| surcharge_details.final_amount)
            .unwrap_or(payment_data.payment_attempt.amount);

        let client_source = header_payload
            .client_source
            .clone()
            .or(payment_data.payment_attempt.client_source.clone());
        let client_version = header_payload
            .client_version
            .clone()
            .or(payment_data.payment_attempt.client_version.clone());

        let m_payment_data_payment_attempt = payment_data.payment_attempt.clone();
        let m_payment_method_id =
            payment_data
                .payment_attempt
                .payment_method_id
                .clone()
                .or(payment_data
                    .payment_method_info
                    .as_ref()
                    .map(|payment_method| payment_method.payment_method_id.clone()));
        let m_browser_info = browser_info.clone();
        let m_connector = connector.clone();
        let m_capture_method = capture_method;
        let m_payment_token = payment_token.clone();
        let m_additional_pm_data = additional_pm_data.clone().or(encode_additional_pm_to_value);
        let m_business_sub_label = business_sub_label.clone();
        let m_straight_through_algorithm = straight_through_algorithm.clone();
        let m_error_code = error_code.clone();
        let m_error_message = error_message.clone();
        let m_fingerprint_id = payment_data.payment_attempt.fingerprint_id.clone();
        let m_db = state.clone().store;
        let surcharge_amount = payment_data
            .surcharge_details
            .as_ref()
            .map(|surcharge_details| surcharge_details.surcharge_amount);
        let tax_amount = payment_data
            .surcharge_details
            .as_ref()
            .map(|surcharge_details| surcharge_details.tax_on_surcharge_amount);

        let (
            external_three_ds_authentication_attempted,
            authentication_connector,
            authentication_id,
        ) = match payment_data.authentication.as_ref() {
            Some(authentication) => (
                Some(authentication.is_separate_authn_required()),
                Some(authentication.authentication_connector.clone()),
                Some(authentication.authentication_id.clone()),
            ),
            None => (None, None, None),
        };

        let payment_attempt_fut = tokio::spawn(
            async move {
                m_db.update_payment_attempt_with_attempt_id(
                    m_payment_data_payment_attempt,
                    storage::PaymentAttemptUpdate::ConfirmUpdate {
                        amount: payment_data.payment_attempt.amount,
                        currency: payment_data.currency,
                        status: attempt_status,
                        payment_method,
                        authentication_type,
                        capture_method: m_capture_method,
                        browser_info: m_browser_info,
                        connector: m_connector,
                        payment_token: m_payment_token,
                        payment_method_data: m_additional_pm_data,
                        payment_method_type,
                        payment_experience,
                        business_sub_label: m_business_sub_label,
                        straight_through_algorithm: m_straight_through_algorithm,
                        error_code: m_error_code,
                        error_message: m_error_message,
                        amount_capturable: Some(authorized_amount),
                        updated_by: storage_scheme.to_string(),
                        merchant_connector_id,
                        surcharge_amount,
                        tax_amount,
                        external_three_ds_authentication_attempted,
                        authentication_connector,
                        authentication_id,
                        payment_method_billing_address_id,
                        fingerprint_id: m_fingerprint_id,
                        payment_method_id: m_payment_method_id,
                        client_source,
                        client_version,
                    },
                    storage_scheme,
                )
                .map(|x| x.to_not_found_response(errors::ApiErrorResponse::PaymentNotFound))
                .await
            }
            .in_current_span(),
        );

        let m_payment_data_payment_intent = payment_data.payment_intent.clone();
        let m_customer_id = customer_id.clone();
        let m_shipping_address_id = shipping_address_id.clone();
        let m_billing_address_id = billing_address_id.clone();
        let m_return_url = return_url.clone();
        let m_business_label = business_label.clone();
        let m_description = description.clone();
        let m_statement_descriptor_name = statement_descriptor_name.clone();
        let m_statement_descriptor_suffix = statement_descriptor_suffix.clone();
        let m_order_details = order_details.clone();
        let m_metadata = metadata.clone();
        let m_frm_metadata = frm_metadata.clone();
        let m_db = state.clone().store;
        let m_storage_scheme = storage_scheme.to_string();
        let session_expiry = m_payment_data_payment_intent.session_expiry;
        let m_key_store = key_store.clone();

        let payment_intent_fut = tokio::spawn(
            async move {
                m_db.update_payment_intent(
                    m_payment_data_payment_intent,
                    storage::PaymentIntentUpdate::Update {
                        amount: payment_data.payment_intent.amount,
                        currency: payment_data.currency,
                        setup_future_usage,
                        status: intent_status,
                        customer_id: m_customer_id,
                        shipping_address_id: m_shipping_address_id,
                        billing_address_id: m_billing_address_id,
                        return_url: m_return_url,
                        business_country,
                        business_label: m_business_label,
                        description: m_description,
                        statement_descriptor_name: m_statement_descriptor_name,
                        statement_descriptor_suffix: m_statement_descriptor_suffix,
                        order_details: m_order_details,
                        metadata: m_metadata,
                        payment_confirm_source: header_payload.payment_confirm_source,
                        updated_by: m_storage_scheme,
                        fingerprint_id: None,
                        session_expiry,
                        request_external_three_ds_authentication: None,
                        frm_metadata: m_frm_metadata,
                        customer_details,
                        merchant_order_reference_id: None,
                    },
                    &m_key_store,
                    storage_scheme,
                )
                .map(|x| x.to_not_found_response(errors::ApiErrorResponse::PaymentNotFound))
                .await
            }
            .in_current_span(),
        );

        let customer_fut =
            if let Some((updated_customer, customer)) = updated_customer.zip(customer) {
                let m_customer_customer_id = customer.customer_id.to_owned();
                let m_customer_merchant_id = customer.merchant_id.to_owned();
                let m_key_store = key_store.clone();
                let m_updated_customer = updated_customer.clone();
                let m_db = state.clone().store;
                tokio::spawn(
                    async move {
                        m_db.update_customer_by_customer_id_merchant_id(
                            m_customer_customer_id,
                            m_customer_merchant_id,
                            customer,
                            m_updated_customer,
                            &m_key_store,
                            storage_scheme,
                        )
                        .await
                        .change_context(errors::ApiErrorResponse::InternalServerError)
                        .attach_printable("Failed to update CustomerConnector in customer")?;

                        Ok::<_, error_stack::Report<errors::ApiErrorResponse>>(())
                    }
                    .in_current_span(),
                )
            } else {
                tokio::spawn(
                    async move { Ok::<_, error_stack::Report<errors::ApiErrorResponse>>(()) }
                        .in_current_span(),
                )
            };

        let (payment_intent, payment_attempt, _) = tokio::try_join!(
            utils::flatten_join_error(payment_intent_fut),
            utils::flatten_join_error(payment_attempt_fut),
            utils::flatten_join_error(customer_fut)
        )?;

        payment_data.payment_intent = payment_intent;
        payment_data.payment_attempt = payment_attempt;

        let client_src = payment_data.payment_attempt.client_source.clone();
        let client_ver = payment_data.payment_attempt.client_version.clone();

        let frm_message = payment_data.frm_message.clone();
        req_state
            .event_context
            .event(AuditEvent::new(AuditEventType::PaymentConfirm {
                client_src,
                client_ver,
                frm_message,
            }))
            .with(payment_data.to_event())
            .emit();
        Ok((Box::new(self), payment_data))
    }
}

impl<F: Send + Clone> ValidateRequest<F, api::PaymentsRequest> for PaymentConfirm {
    #[instrument(skip_all)]
    fn validate_request<'a, 'b>(
        &'b self,
        request: &api::PaymentsRequest,
        merchant_account: &'a domain::MerchantAccount,
    ) -> RouterResult<(
        BoxedOperation<'b, F, api::PaymentsRequest>,
        operations::ValidateResult<'a>,
    )> {
        helpers::validate_customer_details_in_request(request)?;
        if let Some(amount) = request.amount {
            helpers::validate_max_amount(amount)?;
        }

        let request_merchant_id = request.merchant_id.as_deref();
        helpers::validate_merchant_id(&merchant_account.merchant_id, request_merchant_id)
            .change_context(errors::ApiErrorResponse::InvalidDataFormat {
                field_name: "merchant_id".to_string(),
                expected_format: "merchant_id from merchant account".to_string(),
            })?;

        helpers::validate_payment_method_fields_present(request)?;

        let _mandate_type =
            helpers::validate_mandate(request, payments::is_operation_confirm(self))?;

        helpers::validate_recurring_details_and_token(
            &request.recurring_details,
            &request.payment_token,
            &request.mandate_id,
        )?;

        let payment_id = request
            .payment_id
            .clone()
            .ok_or(report!(errors::ApiErrorResponse::PaymentNotFound))?;

        let _request_straight_through: Option<api::routing::StraightThroughAlgorithm> = request
            .routing
            .clone()
            .map(|val| val.parse_value("RoutingAlgorithm"))
            .transpose()
            .change_context(errors::ApiErrorResponse::InvalidRequestData {
                message: "Invalid straight through routing rules format".to_string(),
            })
            .attach_printable("Invalid straight through routing rules format")?;

        Ok((
            Box::new(self),
            operations::ValidateResult {
                merchant_id: &merchant_account.merchant_id,
                payment_id: payment_id.and_then(|id| core_utils::validate_id(id, "payment_id"))?,
                storage_scheme: merchant_account.storage_scheme,
                requeue: matches!(
                    request.retry_action,
                    Some(api_models::enums::RetryAction::Requeue)
                ),
            },
        ))
    }
}
