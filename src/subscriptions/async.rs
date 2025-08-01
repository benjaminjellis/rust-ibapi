//! Asynchronous subscription implementation

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use log::{debug, warn};
use tokio::sync::mpsc;

use super::common::{process_decode_result, ProcessingResult};
use super::{ResponseContext, StreamDecoder};
use crate::client::r#async::Client;
use crate::messages::{OutgoingMessages, RequestMessage, ResponseMessage};
use crate::transport::AsyncInternalSubscription;
use crate::Error;

// Type aliases to reduce complexity
type CancelFn = Box<dyn Fn(i32, Option<i32>, Option<&ResponseContext>) -> Result<RequestMessage, Error> + Send + Sync>;
type DecoderFn<T> = Arc<dyn Fn(i32, &mut ResponseMessage) -> Result<T, Error> + Send + Sync>;

/// Asynchronous subscription for streaming data
pub struct Subscription<T> {
    inner: SubscriptionInner<T>,
    /// Metadata for cancellation
    request_id: Option<i32>,
    order_id: Option<i32>,
    _message_type: Option<OutgoingMessages>,
    response_context: ResponseContext,
    cancelled: Arc<AtomicBool>,
    client: Option<Arc<Client>>,
    /// Cancel message generator
    cancel_fn: Option<Arc<CancelFn>>,
}

enum SubscriptionInner<T> {
    /// Subscription with decoder - receives ResponseMessage and decodes to T
    WithDecoder {
        subscription: AsyncInternalSubscription,
        decoder: DecoderFn<T>,
        client: Arc<Client>,
    },
    /// Pre-decoded subscription - receives T directly
    PreDecoded { receiver: mpsc::UnboundedReceiver<Result<T, Error>> },
}

impl<T> Clone for SubscriptionInner<T> {
    fn clone(&self) -> Self {
        match self {
            SubscriptionInner::WithDecoder {
                subscription,
                decoder,
                client,
            } => SubscriptionInner::WithDecoder {
                subscription: subscription.clone(),
                decoder: decoder.clone(),
                client: client.clone(),
            },
            SubscriptionInner::PreDecoded { .. } => {
                // Can't clone mpsc receivers
                panic!("Cannot clone pre-decoded subscriptions");
            }
        }
    }
}

impl<T> Clone for Subscription<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            request_id: self.request_id,
            order_id: self.order_id,
            _message_type: self._message_type,
            response_context: self.response_context.clone(),
            cancelled: self.cancelled.clone(),
            client: self.client.clone(),
            cancel_fn: self.cancel_fn.clone(),
        }
    }
}

impl<T> Subscription<T> {
    /// Create a subscription from an internal subscription and a decoder
    pub fn with_decoder<D>(
        internal: AsyncInternalSubscription,
        client: Arc<Client>,
        decoder: D,
        request_id: Option<i32>,
        order_id: Option<i32>,
        message_type: Option<OutgoingMessages>,
        response_context: ResponseContext,
    ) -> Self
    where
        D: Fn(i32, &mut ResponseMessage) -> Result<T, Error> + Send + Sync + 'static,
    {
        Self {
            inner: SubscriptionInner::WithDecoder {
                subscription: internal,
                decoder: Arc::new(decoder),
                client: client.clone(),
            },
            request_id,
            order_id,
            _message_type: message_type,
            response_context,
            cancelled: Arc::new(AtomicBool::new(false)),
            client: Some(client),
            cancel_fn: None,
        }
    }

    /// Create a subscription from an internal subscription with a decoder function
    pub fn new_with_decoder<F>(
        internal: AsyncInternalSubscription,
        client: Arc<Client>,
        decoder: F,
        request_id: Option<i32>,
        order_id: Option<i32>,
        message_type: Option<OutgoingMessages>,
        response_context: ResponseContext,
    ) -> Self
    where
        F: Fn(i32, &mut ResponseMessage) -> Result<T, Error> + Send + Sync + 'static,
    {
        Self::with_decoder(internal, client, decoder, request_id, order_id, message_type, response_context)
    }

    /// Create a subscription from an internal subscription using the DataStream decoder
    pub(crate) fn new_from_internal<D>(
        internal: AsyncInternalSubscription,
        client: Arc<Client>,
        request_id: Option<i32>,
        order_id: Option<i32>,
        message_type: Option<OutgoingMessages>,
        response_context: ResponseContext,
    ) -> Self
    where
        D: StreamDecoder<T> + 'static,
        T: 'static,
    {
        let mut sub = Self::with_decoder(internal, client.clone(), D::decode, request_id, order_id, message_type, response_context);
        // Store the cancel function
        sub.cancel_fn = Some(Arc::new(Box::new(D::cancel_message)));
        sub
    }

    /// Create a subscription from internal subscription without explicit metadata (for backward compatibility)
    pub(crate) fn new_from_internal_simple<D>(internal: AsyncInternalSubscription, client: Arc<Client>) -> Self
    where
        D: StreamDecoder<T> + 'static,
        T: 'static,
    {
        // The AsyncInternalSubscription already has cleanup logic, so we don't need cancel metadata
        Self::new_from_internal::<D>(internal, client, None, None, None, ResponseContext::default())
    }

    /// Create subscription from existing receiver (for backward compatibility)
    pub fn new(receiver: mpsc::UnboundedReceiver<Result<T, Error>>) -> Self {
        // This creates a subscription that expects pre-decoded messages
        // Used for compatibility with existing code that manually decodes
        Self {
            inner: SubscriptionInner::PreDecoded { receiver },
            request_id: None,
            order_id: None,
            _message_type: None,
            response_context: ResponseContext::default(),
            cancelled: Arc::new(AtomicBool::new(false)),
            client: None,
            cancel_fn: None,
        }
    }

    /// Get the next value from the subscription
    pub async fn next(&mut self) -> Option<Result<T, Error>>
    where
        T: 'static,
    {
        match &mut self.inner {
            SubscriptionInner::WithDecoder {
                subscription,
                decoder,
                client,
            } => loop {
                match subscription.next().await {
                    Some(mut message) => {
                        let result = decoder(client.server_version(), &mut message);
                        match process_decode_result(result) {
                            ProcessingResult::Success(val) => return Some(Ok(val)),
                            ProcessingResult::EndOfStream => return None,
                            ProcessingResult::Retry => continue,
                            ProcessingResult::Error(err) => return Some(Err(err)),
                        }
                    }
                    None => return None,
                }
            },
            SubscriptionInner::PreDecoded { receiver } => receiver.recv().await,
        }
    }
}

impl<T> Subscription<T> {
    /// Cancel the subscription
    pub async fn cancel(&self) {
        if self.cancelled.load(Ordering::Relaxed) {
            return;
        }

        self.cancelled.store(true, Ordering::Relaxed);

        if let (Some(client), Some(cancel_fn)) = (&self.client, &self.cancel_fn) {
            let id = self.request_id.or(self.order_id);
            if let Ok(message) = cancel_fn(client.server_version(), id, Some(&self.response_context)) {
                if let Err(e) = client.message_bus.send_message(message).await {
                    warn!("error sending cancel message: {e}")
                }
            }
        }

        // The AsyncInternalSubscription's Drop will handle cleanup
    }
}

impl<T> Drop for Subscription<T> {
    fn drop(&mut self) {
        debug!("dropping async subscription");

        // Check if already cancelled
        if self.cancelled.load(Ordering::Relaxed) {
            return;
        }

        self.cancelled.store(true, Ordering::Relaxed);

        // Try to send cancel message if we have the necessary components
        if let (Some(client), Some(cancel_fn)) = (&self.client, &self.cancel_fn) {
            let client = client.clone();
            let id = self.request_id.or(self.order_id);
            let response_context = self.response_context.clone();
            let server_version = client.server_version();

            // Clone the cancel function for use in the spawned task
            if let Ok(message) = cancel_fn(server_version, id, Some(&response_context)) {
                // Spawn a task to send the cancel message since drop can't be async
                tokio::spawn(async move {
                    if let Err(e) = client.message_bus.send_message(message).await {
                        warn!("error sending cancel message in drop: {e}");
                    }
                });
            }
        }

        // The AsyncInternalSubscription's Drop will handle channel cleanup
    }
}

// Note: Stream trait implementation removed because tokio's broadcast::Receiver
// doesn't provide poll_recv. Users should use the async next() method instead.
// If Stream is needed, users can convert using futures::stream::unfold.
