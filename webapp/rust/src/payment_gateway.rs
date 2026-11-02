use crate::Error;
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

pub async fn request_payment_gateway_post_payment(
    payment_gateway_url: &str,
    token: &str,
    amount: i32,
    idempotency_key: &str,
    ride_count: usize,
) -> Result<(), Error> {
    let mut retry = 0;
    let client = http_client();
    let param = PaymentGatewayPostPaymentRequest { amount };

    loop {
        let result = async {
            let res = client
                .post(format!("{payment_gateway_url}/payments"))
                .bearer_auth(token)
                .header("Idempotency-Key", idempotency_key)
                .json(&param)
                .send()
                .await
                .map_err(PaymentGatewayError::Reqwest)?;

            if res.status() != reqwest::StatusCode::NO_CONTENT {
                let get_res = client
                    .get(format!("{payment_gateway_url}/payments"))
                    .bearer_auth(token)
                    .send()
                    .await
                    .map_err(PaymentGatewayError::Reqwest)?;

                if get_res.status() != reqwest::StatusCode::OK {
                    return Err(PaymentGatewayError::GetPayment(get_res.status()).into());
                }
                let payments: Vec<PaymentGatewayGetPaymentsResponseOne> =
                    get_res.json().await.map_err(PaymentGatewayError::Reqwest)?;

                if ride_count != payments.len() {
                    return Err(PaymentGatewayError::UnexpectedNumberOfPayments {
                        ride_count,
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
