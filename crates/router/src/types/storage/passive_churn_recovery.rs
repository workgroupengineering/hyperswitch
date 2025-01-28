use std::fmt::Debug;

use api_models::payments::{PaymentsGetIntentRequest, PaymentsRetrieveRequest};
use common_utils::id_type;
use hyperswitch_domain_models::{business_profile, merchant_account, merchant_key_store};
#[derive(serde::Serialize, serde::Deserialize)]
pub struct PCRExecuteWorkflowTrackingData {
    pub request: PaymentsGetIntentRequest,
    pub merchant_id: id_type::MerchantId,
    pub profile_id: id_type::ProfileId,
    pub global_payment_id: id_type::GlobalPaymentId,
    pub platform_merchant_id: Option<id_type::MerchantId>,
}

#[derive(serde::Deserialize, serde::Serialize, Debug)]
pub struct PCRPsyncWorkflowTrackingData {
    pub global_payment_id: id_type::GlobalPaymentId,
    pub merchant_id: id_type::MerchantId,
    pub request: PaymentsRetrieveRequest,
}

#[derive(Debug, Clone)]
pub struct PCRPaymentData {
    pub merchant_account: merchant_account::MerchantAccount,
    pub profile: business_profile::Profile,
    pub platform_merchant_account: Option<merchant_account::MerchantAccount>,
    pub key_store: merchant_key_store::MerchantKeyStore,
    pub global_payment_id: id_type::GlobalPaymentId,
}
