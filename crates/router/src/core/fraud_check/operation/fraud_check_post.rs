use async_trait::async_trait;
use common_enums::{CaptureMethod, FrmSuggestion};
use common_utils::ext_traits::Encode;
use hyperswitch_domain_models::payments::{
    payment_attempt::PaymentAttemptUpdate, payment_intent::PaymentIntentUpdate, HeaderPayload,
};
use router_env::{instrument, logger, tracing};

use super::{Domain, FraudCheckOperation, GetTracker, UpdateTracker};
use crate::{
    consts,
    core::{
        errors::{RouterResult, StorageErrorExt},
        fraud_check::{
            self as frm_core,
            types::{FrmData, PaymentDetails, PaymentToFrmData, CANCEL_INITIATED},
            ConnectorDetailsCore, FrmConfigsObject,
        },
        payments,
    },
    errors,
    routes::app::ReqState,
    services::{self, api},
    types::{
        api::{
            enums::{AttemptStatus, IntentStatus},
            fraud_check as frm_api, payments as payment_types, Capture, Void,
        },
        domain,
        fraud_check::{
            FraudCheckResponseData, FraudCheckSaleData, FrmRequest, FrmResponse, FrmRouterData,
        },
        storage::{
            enums::{FraudCheckLastStep, FraudCheckStatus, FraudCheckType, MerchantDecision},
            fraud_check::{FraudCheckNew, FraudCheckUpdate},
        },
        ResponseId,
    },
    utils, SessionState,
};

#[derive(Debug, Clone, Copy)]
pub struct FraudCheckPost;

impl<F, D> FraudCheckOperation<F, D> for &FraudCheckPost
where
    F: Clone + Send,
    D: payments::OperationSessionGetters<F>
        + payments::OperationSessionSetters<F>
        + Send
        + Sync
        + Clone,
{
    fn to_get_tracker(&self) -> RouterResult<&(dyn GetTracker<PaymentToFrmData> + Send + Sync)> {
        Ok(*self)
    }
    fn to_domain(&self) -> RouterResult<&(dyn Domain<F, D>)> {
        Ok(*self)
    }
    fn to_update_tracker(&self) -> RouterResult<&(dyn UpdateTracker<FrmData, F, D> + Send + Sync)> {
        Ok(*self)
    }
}

impl<F, D> FraudCheckOperation<F, D> for FraudCheckPost
where
    F: Clone + Send,
    D: payments::OperationSessionGetters<F>
        + payments::OperationSessionSetters<F>
        + Send
        + Sync
        + Clone,
{
    fn to_get_tracker(&self) -> RouterResult<&(dyn GetTracker<PaymentToFrmData> + Send + Sync)> {
        Ok(self)
    }
    fn to_domain(&self) -> RouterResult<&(dyn Domain<F, D>)> {
        Ok(self)
    }
    fn to_update_tracker(&self) -> RouterResult<&(dyn UpdateTracker<FrmData, F, D> + Send + Sync)> {
        Ok(self)
    }
}

#[async_trait]
impl GetTracker<PaymentToFrmData> for FraudCheckPost {
    #[cfg(feature = "v2")]
    async fn get_trackers<'a>(
        &'a self,
        state: &'a SessionState,
        payment_data: PaymentToFrmData,
        frm_connector_details: ConnectorDetailsCore,
    ) -> RouterResult<Option<FrmData>> {
        todo!()
    }

    #[cfg(feature = "v1")]
    #[instrument(skip_all)]
    async fn get_trackers<'a>(
        &'a self,
        state: &'a SessionState,
        payment_data: PaymentToFrmData,
        frm_connector_details: ConnectorDetailsCore,
    ) -> RouterResult<Option<FrmData>> {
        let db = &*state.store;

        let payment_details: Option<serde_json::Value> = PaymentDetails::from(payment_data.clone())
            .encode_to_value()
            .ok();
        let existing_fraud_check = db
            .find_fraud_check_by_payment_id_if_present(
                payment_data.payment_intent.get_id().to_owned(),
                payment_data.merchant_account.get_id().clone(),
            )
            .await
            .ok();
        let fraud_check = match existing_fraud_check {
            Some(Some(fraud_check)) => Ok(fraud_check),
            _ => {
                db.insert_fraud_check_response(FraudCheckNew {
                    frm_id: utils::generate_id(consts::ID_LENGTH, "frm"),
                    payment_id: payment_data.payment_intent.get_id().to_owned(),
                    merchant_id: payment_data.merchant_account.get_id().clone(),
                    attempt_id: payment_data.payment_attempt.attempt_id.clone(),
                    created_at: common_utils::date_time::now(),
                    frm_name: frm_connector_details.connector_name,
                    frm_transaction_id: None,
                    frm_transaction_type: FraudCheckType::PostFrm,
                    frm_status: FraudCheckStatus::Pending,
                    frm_score: None,
                    frm_reason: None,
                    frm_error: None,
                    payment_details,
                    metadata: None,
                    modified_at: common_utils::date_time::now(),
                    last_step: FraudCheckLastStep::Processing,
                    payment_capture_method: payment_data.payment_attempt.capture_method,
                })
                .await
            }
        };
        match fraud_check {
            Ok(fraud_check_value) => {
                let frm_data = FrmData {
                    payment_intent: payment_data.payment_intent,
                    payment_attempt: payment_data.payment_attempt,
                    merchant_account: payment_data.merchant_account,
                    address: payment_data.address,
                    fraud_check: fraud_check_value,
                    connector_details: payment_data.connector_details,
                    order_details: payment_data.order_details,
                    refund: None,
                    frm_metadata: payment_data.frm_metadata,
                };
                Ok(Some(frm_data))
            }
            Err(error) => {
                router_env::logger::error!("inserting into fraud_check table failed {error:?}");
                Ok(None)
            }
        }
    }
}

#[async_trait]
impl<F, D> Domain<F, D> for FraudCheckPost
where
    F: Clone + Send,
    D: payments::OperationSessionGetters<F>
        + payments::OperationSessionSetters<F>
        + Send
        + Sync
        + Clone,
{
    #[instrument(skip_all)]
    async fn post_payment_frm<'a>(
        &'a self,
        state: &'a SessionState,
        _req_state: ReqState,
        payment_data: &mut D,
        frm_data: &mut FrmData,
        merchant_context: &domain::MerchantContext,
        customer: &Option<domain::Customer>,
    ) -> RouterResult<Option<FrmRouterData>> {
        if frm_data.fraud_check.last_step != FraudCheckLastStep::Processing {
            logger::debug!("post_flow::Sale Skipped");
            return Ok(None);
        }
        let router_data = frm_core::call_frm_service::<F, frm_api::Sale, _, D>(
            state,
            payment_data,
            &mut frm_data.to_owned(),
            merchant_context,
            customer,
        )
        .await?;
        frm_data.fraud_check.last_step = FraudCheckLastStep::CheckoutOrSale;
        Ok(Some(FrmRouterData {
            merchant_id: router_data.merchant_id,
            connector: router_data.connector,
            payment_id: router_data.payment_id.clone(),
            attempt_id: router_data.attempt_id,
            request: FrmRequest::Sale(FraudCheckSaleData {
                amount: router_data.request.amount,
                order_details: router_data.request.order_details,
                currency: router_data.request.currency,
                email: router_data.request.email,
            }),
            response: FrmResponse::Sale(router_data.response),
        }))
    }

    #[cfg(feature = "v2")]
    #[instrument(skip_all)]
    async fn execute_post_tasks(
        &self,
        _state: &SessionState,
        _req_state: ReqState,
        _frm_data: &mut FrmData,
        _merchant_context: &domain::MerchantContext,
        _frm_configs: FrmConfigsObject,
        _frm_suggestion: &mut Option<FrmSuggestion>,
        _payment_data: &mut D,
        _customer: &Option<domain::Customer>,
        _should_continue_capture: &mut bool,
    ) -> RouterResult<Option<FrmData>> {
        todo!()
    }

    #[cfg(feature = "v1")]
    #[instrument(skip_all)]
    async fn execute_post_tasks(
        &self,
        state: &SessionState,
        req_state: ReqState,
        frm_data: &mut FrmData,
        merchant_context: &domain::MerchantContext,
        _frm_configs: FrmConfigsObject,
        frm_suggestion: &mut Option<FrmSuggestion>,
        payment_data: &mut D,
        customer: &Option<domain::Customer>,
        _should_continue_capture: &mut bool,
    ) -> RouterResult<Option<FrmData>> {
        if matches!(frm_data.fraud_check.frm_status, FraudCheckStatus::Fraud)
            && matches!(
                frm_data.fraud_check.last_step,
                FraudCheckLastStep::CheckoutOrSale
            )
        {
            *frm_suggestion = Some(FrmSuggestion::FrmCancelTransaction);

            let cancel_req = api_models::payments::PaymentsCancelRequest {
                payment_id: frm_data.payment_intent.get_id().to_owned(),
                cancellation_reason: frm_data.fraud_check.frm_error.clone(),
                merchant_connector_details: None,
            };
            let cancel_res = Box::pin(payments::payments_core::<
                Void,
                payment_types::PaymentsResponse,
                _,
                _,
                _,
                payments::PaymentData<Void>,
            >(
                state.clone(),
                req_state.clone(),
                merchant_context.clone(),
                None,
                payments::PaymentCancel,
                cancel_req,
                api::AuthFlow::Merchant,
                payments::CallConnectorAction::Trigger,
                None,
                HeaderPayload::default(),
            ))
            .await?;
            logger::debug!("payment_id : {:?} has been cancelled since it has been found fraudulent by configured frm connector",payment_data.get_payment_attempt().payment_id);
            if let services::ApplicationResponse::JsonWithHeaders((payments_response, _)) =
                cancel_res
            {
                payment_data.set_payment_intent_status(payments_response.status);
            }
            let _router_data = frm_core::call_frm_service::<F, frm_api::RecordReturn, _, D>(
                state,
                payment_data,
                &mut frm_data.to_owned(),
                merchant_context,
                customer,
            )
            .await?;
            frm_data.fraud_check.last_step = FraudCheckLastStep::TransactionOrRecordRefund;
        } else if matches!(
            frm_data.fraud_check.frm_status,
            FraudCheckStatus::ManualReview
        ) {
            *frm_suggestion = Some(FrmSuggestion::FrmManualReview);
        } else if matches!(frm_data.fraud_check.frm_status, FraudCheckStatus::Legit)
            && matches!(
                frm_data.fraud_check.payment_capture_method,
                Some(CaptureMethod::Automatic) | Some(CaptureMethod::SequentialAutomatic)
            )
        {
            let capture_request = api_models::payments::PaymentsCaptureRequest {
                payment_id: frm_data.payment_intent.get_id().to_owned(),
                merchant_id: None,
                amount_to_capture: None,
                refund_uncaptured_amount: None,
                statement_descriptor_suffix: None,
                statement_descriptor_prefix: None,
                merchant_connector_details: None,
            };
            let capture_response = Box::pin(payments::payments_core::<
                Capture,
                payment_types::PaymentsResponse,
                _,
                _,
                _,
                payments::PaymentData<Capture>,
            >(
                state.clone(),
                req_state.clone(),
                merchant_context.clone(),
                None,
                payments::PaymentCapture,
                capture_request,
                api::AuthFlow::Merchant,
                payments::CallConnectorAction::Trigger,
                None,
                HeaderPayload::default(),
            ))
            .await?;
            logger::debug!("payment_id : {:?} has been captured since it has been found legit by configured frm connector",payment_data.get_payment_attempt().payment_id);
            if let services::ApplicationResponse::JsonWithHeaders((payments_response, _)) =
                capture_response
            {
                payment_data.set_payment_intent_status(payments_response.status);
            }
        };
        return Ok(Some(frm_data.to_owned()));
    }

    #[instrument(skip_all)]
    async fn pre_payment_frm<'a>(
        &'a self,
        state: &'a SessionState,
        payment_data: &mut D,
        frm_data: &mut FrmData,
        merchant_context: &domain::MerchantContext,
        customer: &Option<domain::Customer>,
    ) -> RouterResult<FrmRouterData> {
        let router_data = frm_core::call_frm_service::<F, frm_api::Sale, _, D>(
            state,
            payment_data,
            &mut frm_data.to_owned(),
            merchant_context,
            customer,
        )
        .await?;
        Ok(FrmRouterData {
            merchant_id: router_data.merchant_id,
            connector: router_data.connector,
            payment_id: router_data.payment_id,
            attempt_id: router_data.attempt_id,
            request: FrmRequest::Sale(FraudCheckSaleData {
                amount: router_data.request.amount,
                order_details: router_data.request.order_details,
                currency: router_data.request.currency,
                email: router_data.request.email,
            }),
            response: FrmResponse::Sale(router_data.response),
        })
    }
}

#[async_trait]
impl<F, D> UpdateTracker<FrmData, F, D> for FraudCheckPost
where
    F: Clone + Send,
    D: payments::OperationSessionGetters<F>
        + payments::OperationSessionSetters<F>
        + Send
        + Sync
        + Clone,
{
    #[cfg(feature = "v2")]
    async fn update_tracker<'b>(
        &'b self,
        state: &SessionState,
        key_store: &domain::MerchantKeyStore,
        mut frm_data: FrmData,
        payment_data: &mut D,
        frm_suggestion: Option<FrmSuggestion>,
        frm_router_data: FrmRouterData,
    ) -> RouterResult<FrmData> {
        todo!()
    }

    #[cfg(feature = "v1")]
    async fn update_tracker<'b>(
        &'b self,
        state: &SessionState,
        key_store: &domain::MerchantKeyStore,
        mut frm_data: FrmData,
        payment_data: &mut D,
        frm_suggestion: Option<FrmSuggestion>,
        frm_router_data: FrmRouterData,
    ) -> RouterResult<FrmData> {
        let db = &*state.store;
        let key_manager_state = &state.into();
        let frm_check_update = match frm_router_data.response {
            FrmResponse::Sale(response) => match response {
                Err(err) => Some(FraudCheckUpdate::ErrorUpdate {
                    status: FraudCheckStatus::TransactionFailure,
                    error_message: Some(Some(err.message)),
                }),
                Ok(payments_response) => match payments_response {
                    FraudCheckResponseData::TransactionResponse {
                        resource_id,
                        connector_metadata,
                        status,
                        reason,
                        score,
                    } => {
                        let connector_transaction_id = match resource_id {
                            ResponseId::NoResponseId => None,
                            ResponseId::ConnectorTransactionId(id) => Some(id),
                            ResponseId::EncodedData(id) => Some(id),
                        };

                        let fraud_check_update = FraudCheckUpdate::ResponseUpdate {
                            frm_status: status,
                            frm_transaction_id: connector_transaction_id,
                            frm_reason: reason,
                            frm_score: score,
                            metadata: connector_metadata,
                            modified_at: common_utils::date_time::now(),
                            last_step: frm_data.fraud_check.last_step,
                            payment_capture_method: frm_data.fraud_check.payment_capture_method,
                        };
                        Some(fraud_check_update)
                    },
                    FraudCheckResponseData::RecordReturnResponse { resource_id: _, connector_metadata: _, return_id: _ } => {
                        Some(FraudCheckUpdate::ErrorUpdate {
                            status: FraudCheckStatus::TransactionFailure,
                            error_message: Some(Some(
                                "Error: Got Record Return Response response in current Sale flow".to_string(),
                            )),
                        })
                    }
                    FraudCheckResponseData::FulfillmentResponse {
                        order_id: _,
                        shipment_ids: _,
                    } => None,
                },
            },
            FrmResponse::Fulfillment(response) => match response {
                Err(err) => Some(FraudCheckUpdate::ErrorUpdate {
                    status: FraudCheckStatus::TransactionFailure,
                    error_message: Some(Some(err.message)),
                }),
                Ok(payments_response) => match payments_response {
                    FraudCheckResponseData::TransactionResponse {
                        resource_id,
                        connector_metadata,
                        status,
                        reason,
                        score,
                    } => {
                        let connector_transaction_id = match resource_id {
                            ResponseId::NoResponseId => None,
                            ResponseId::ConnectorTransactionId(id) => Some(id),
                            ResponseId::EncodedData(id) => Some(id),
                        };

                        let fraud_check_update = FraudCheckUpdate::ResponseUpdate {
                            frm_status: status,
                            frm_transaction_id: connector_transaction_id,
                            frm_reason: reason,
                            frm_score: score,
                            metadata: connector_metadata,
                            modified_at: common_utils::date_time::now(),
                            last_step: frm_data.fraud_check.last_step,
                            payment_capture_method: frm_data.fraud_check.payment_capture_method,
                        };
                        Some(fraud_check_update)
                    }
                    FraudCheckResponseData::FulfillmentResponse {
                        order_id: _,
                        shipment_ids: _,
                    } => None,
                    FraudCheckResponseData::RecordReturnResponse { resource_id: _, connector_metadata: _, return_id: _ } => None,

                },
            },

            FrmResponse::RecordReturn(response) => match response {
                Err(err) => Some(FraudCheckUpdate::ErrorUpdate {
                    status: FraudCheckStatus::TransactionFailure,
                    error_message: Some(Some(err.message)),
                }),
                Ok(payments_response) => match payments_response {
                    FraudCheckResponseData::TransactionResponse {
                        resource_id: _,
                        connector_metadata: _,
                        status: _,
                        reason: _,
                        score: _,
                    } => {
                        Some(FraudCheckUpdate::ErrorUpdate {
                            status: FraudCheckStatus::TransactionFailure,
                            error_message: Some(Some(
                                "Error: Got Transaction Response response in current Record Return flow".to_string(),
                            )),
                        })
                    },
                    FraudCheckResponseData::FulfillmentResponse {order_id: _, shipment_ids: _ } => {
                        None
                    },
                    FraudCheckResponseData::RecordReturnResponse { resource_id, connector_metadata, return_id: _ } => {
                        let connector_transaction_id = match resource_id {
                            ResponseId::NoResponseId => None,
                            ResponseId::ConnectorTransactionId(id) => Some(id),
                            ResponseId::EncodedData(id) => Some(id),
                        };

                        let fraud_check_update = FraudCheckUpdate::ResponseUpdate {
                            frm_status: frm_data.fraud_check.frm_status,
                            frm_transaction_id: connector_transaction_id,
                            frm_reason: frm_data.fraud_check.frm_reason.clone(),
                            frm_score: frm_data.fraud_check.frm_score,
                            metadata: connector_metadata,
                            modified_at: common_utils::date_time::now(),
                            last_step: frm_data.fraud_check.last_step,
                            payment_capture_method: frm_data.fraud_check.payment_capture_method,
                        };
                        Some(fraud_check_update)
                    }
                },
            },

            FrmResponse::Checkout(_) | FrmResponse::Transaction(_) => {
                Some(FraudCheckUpdate::ErrorUpdate {
                    status: FraudCheckStatus::TransactionFailure,
                    error_message: Some(Some(
                        "Error: Got Pre(Sale) flow response in current post flow".to_string(),
                    )),
                })
            }
        };

        if let Some(frm_suggestion) = frm_suggestion {
            let (payment_attempt_status, payment_intent_status, merchant_decision, error_message) =
                match frm_suggestion {
                    FrmSuggestion::FrmCancelTransaction => (
                        AttemptStatus::Failure,
                        IntentStatus::Failed,
                        Some(MerchantDecision::Rejected.to_string()),
                        Some(Some(CANCEL_INITIATED.to_string())),
                    ),
                    FrmSuggestion::FrmManualReview => (
                        AttemptStatus::Unresolved,
                        IntentStatus::RequiresMerchantAction,
                        None,
                        None,
                    ),
                    FrmSuggestion::FrmAuthorizeTransaction => (
                        AttemptStatus::Authorized,
                        IntentStatus::RequiresCapture,
                        None,
                        None,
                    ),
                };

            let payment_attempt_update = PaymentAttemptUpdate::RejectUpdate {
                status: payment_attempt_status,
                error_code: Some(Some(frm_data.fraud_check.frm_status.to_string())),
                error_message,
                updated_by: frm_data.merchant_account.storage_scheme.to_string(),
            };

            #[cfg(feature = "v1")]
            let payment_attempt = db
                .update_payment_attempt_with_attempt_id(
                    payment_data.get_payment_attempt().clone(),
                    payment_attempt_update,
                    frm_data.merchant_account.storage_scheme,
                )
                .await
                .to_not_found_response(errors::ApiErrorResponse::PaymentNotFound)?;

            #[cfg(feature = "v2")]
            let payment_attempt = db
                .update_payment_attempt_with_attempt_id(
                    key_manager_state,
                    key_store,
                    payment_data.get_payment_attempt().clone(),
                    payment_attempt_update,
                    frm_data.merchant_account.storage_scheme,
                )
                .await
                .to_not_found_response(errors::ApiErrorResponse::PaymentNotFound)?;

            payment_data.set_payment_attempt(payment_attempt);

            let payment_intent = db
                .update_payment_intent(
                    key_manager_state,
                    payment_data.get_payment_intent().clone(),
                    PaymentIntentUpdate::RejectUpdate {
                        status: payment_intent_status,
                        merchant_decision,
                        updated_by: frm_data.merchant_account.storage_scheme.to_string(),
                    },
                    key_store,
                    frm_data.merchant_account.storage_scheme,
                )
                .await
                .to_not_found_response(errors::ApiErrorResponse::PaymentNotFound)?;

            payment_data.set_payment_intent(payment_intent);
        }
        frm_data.fraud_check = match frm_check_update {
            Some(fraud_check_update) => db
                .update_fraud_check_response_with_attempt_id(
                    frm_data.fraud_check.clone(),
                    fraud_check_update,
                )
                .await
                .map_err(|error| error.change_context(errors::ApiErrorResponse::PaymentNotFound))?,
            None => frm_data.fraud_check.clone(),
        };

        Ok(frm_data)
    }
}
