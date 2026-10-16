---
name: app-store-server
description: App Store Server API reference for the apple plugin — look up in-app purchase transactions, subscription status, order IDs, refunds, server notification history, test notifications, and decode signed JWS payloads (signedPayload, signedTransactionInfo). Load for customer-support investigations on purchases or StoreKit server work (storekit_* tools).
user-invocable: false
---

# App Store Server API (tools `storekit_*`, `apple_jws_decode`)

Auth: ES256 JWT from `app_store_server.key_id/issuer_id/private_key` with `bid` = bundle id
(In-App Purchase key from App Store Connect → Integrations). Hosts:
production `api.storekit.itunes.apple.com`, sandbox `api.storekit-sandbox.itunes.apple.com`
(`environment` param or config). A transaction only exists in one environment: if production
returns 404 `TransactionIdNotFound`, retry with `environment: sandbox`.

All signed fields (`signedTransactionInfo`, `signedRenewalInfo`, `signedPayload`,
`signedTransactions[]`) are decoded automatically (signature not verified — the server trusts
Apple's TLS endpoint; for webhook payloads received from third parties verify the x5c chain
against Apple Root CA G3 before trusting).

## Tasks

- **Customer says "I paid but…"**: ask for the order id from their receipt e-mail →
  `storekit_order_lookup` (status 0 = valid) → transaction ids → `storekit_history` /
  `storekit_subscriptions`.
- **Subscription state**: `storekit_subscriptions` → per group: `status` 1 active, 2 expired,
  3 billing retry, 4 grace period, 5 revoked; `renewalInfo.autoRenewStatus`,
  `expirationIntent`, `gracePeriodExpiresDate`, `priceIncreaseStatus`, offer fields.
- **Refunds**: `storekit_refund_history` (revocationDate/revocationReason on transactions).
- **Did we receive the webhook?** `storekit_notification_history` for a date range
  (ms epoch; ≤180 days back), filter `notificationType` (SUBSCRIBED, DID_RENEW, DID_CHANGE_RENEWAL_STATUS,
  EXPIRED, GRACE_PERIOD_EXPIRED, REFUND, REFUND_DECLINED, CONSUMPTION_REQUEST, RENEWAL_EXTENDED,
  PRICE_INCREASE, OFFER_REDEEMED, ONE_TIME_CHARGE, TEST…) or `transaction_id`;
  `storekit_test_notification` fires a TEST notification to the configured URL and reports delivery.
- **Consumption info for refund requests**: `storekit_request PUT /inApps/v1/transactions/consumption/{transactionId}`
  with `{"customerConsented": true, "consumptionStatus": 1, "platform": 1, "sampleContentProvided": false, "deliveryStatus": 0, "appAccountToken": "", "accountTenure": 4, "playTime": 0, "lifetimeDollarsRefunded": 0, "lifetimeDollarsPurchased": 3, "userStatus": 1, "refundPreference": 0}`.
- **Extend a renewal** (goodwill): `storekit_request PUT /inApps/v1/subscriptions/extend/{originalTransactionId}`
  `{"extendByDays": 7, "extendReasonCode": 1, "requestIdentifier": "<uuid>"}`; mass extension:
  `POST /inApps/v1/subscriptions/extend/mass`.
- **Transaction fields**: `transactionId`, `originalTransactionId`, `webOrderLineItemId`, `productId`,
  `type` (Auto-Renewable Subscription, Non-Consumable, Consumable, Non-Renewing Subscription),
  `purchaseDate/expiresDate` (ms + `_iso`), `environment`, `storefront`, `price` (milli-units) +
  `currency`, `offerType`/`offerDiscountType`, `transactionReason` (PURCHASE, RENEWAL),
  `inAppOwnershipType` (PURCHASED, FAMILY_SHARED), `appAccountToken`.
- `apple_jws_decode` also decodes Sign in with Apple id_tokens and StoreKit 2 `jwsRepresentation`.

Docs: `apple_docs_index technology=appstoreserverapi`.
