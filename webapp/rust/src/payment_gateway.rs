use crate::models::Ride;
use crate::Error;
use std::future::Future;
use std::sync::OnceLock;
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum PaymentGatewayError {
    #[error("reqwest error: {0}")]
    Reqwest(#[from] reqwest::Error),
    #[error("unexpected number of payments: {ride_count} != {payment_count}.")]
    UnexpectedNumberOfPayments {
        ride_count: usize,
        payment_count: usize,
    },
    #[error("[GET /payments] unexpected status code ({0})")]
    GetPayment(reqwest::StatusCode),
}

#[derive(Debug, serde::Serialize)]
pub struct PaymentGatewayPostPaymentRequest {
    pub amount: i32,
}

#[derive(Debug, serde::Deserialize)]
struct PaymentGatewayGetPaymentsResponseOne {
    #[allow(dead_code)]
    amount: i32,
    #[allow(dead_code)]
    status: String,
}

fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("reqwest client")
    })
}

pub trait PostPaymentCallback<'a> {
    type Output: Future<Output = Result<Vec<Ride>, Error>>;

    fn call(&self, tx: &'a mut sqlx::MySqlConnection, user_id: &'a str) -> Self::Output;
}
impl<'a, F, Fut> PostPaymentCallback<'a> for F
where
    F: Fn(&'a mut sqlx::MySqlConnection, &'a str) -> Fut,
    Fut: Future<Output = Result<Vec<Ride>, Error>>,
{
    type Output = Fut;
    fn call(&self, tx: &'a mut sqlx::MySqlConnection, user_id: &'a str) -> Fut {
        self(tx, user_id)
    }
}

pub async fn request_payment_gateway_post_payment<F>(
    payment_gateway_url: &str,
    token: &str,
    param: &PaymentGatewayPostPaymentRequest,
    idempotency_key: &str,
    tx: &mut sqlx::MySqlConnection,
    user_id: &str,
    retrieve_rides_order_by_created_at_asc: F,
) -> Result<(), Error>
where
    F: for<'a> PostPaymentCallback<'a>,
{
    // Retry on failure. Idempotency-Key prevents duplicate charges if the
    // upstream payment service supports it (documented for the contest gateway).
    let mut retry = 0;
    let client = http_client();

    loop {
        let result = async {
            let res = client
                .post(format!("{payment_gateway_url}/payments"))
                .bearer_auth(token)
                .header("Idempotency-Key", idempotency_key)
                .json(param)
                .send()
                .await
                .map_err(PaymentGatewayError::Reqwest)?;

            if res.status() != reqwest::StatusCode::NO_CONTENT {
                // POST may have succeeded even when an error is returned; confirm via GET.
                let get_res = client
                    .get(format!("{payment_gateway_url}/payments"))
                    .bearer_auth(token)
                    .send()
                    .await
                    .map_err(PaymentGatewayError::Reqwest)?;

                // GET /payments returns 200 even during incidents; anything else is fatal.
                if get_res.status() != reqwest::StatusCode::OK {
                    return Err(PaymentGatewayError::GetPayment(get_res.status()).into());
                }
                let payments: Vec<PaymentGatewayGetPaymentsResponseOne> =
                    get_res.json().await.map_err(PaymentGatewayError::Reqwest)?;

                let rides = retrieve_rides_order_by_created_at_asc
                    .call(tx, user_id)
                    .await?;

                if rides.len() != payments.len() {
                    return Err(PaymentGatewayError::UnexpectedNumberOfPayments {
                        ride_count: rides.len(),
                        payment_count: payments.len(),
                    }
                    .into());
                }
            }
            Ok(())
        }
        .await;

        if let Err(err) = result {
            if retry < 5 {
                retry += 1;
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            } else {
                return Err(err);
            }
        }
        break;
    }

    Ok(())
}
