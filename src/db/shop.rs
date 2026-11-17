//! 0027_shop: enums mirroring the CHECK constraints of the shop tables (proved by
//! `tests/it/schema.rs`).

use crate::text_enum;

text_enum!(ShopProductType {
    Food = "food",
    Drink = "drink",
    Cosmetic = "cosmetic",
    Craft = "craft",
    GiftBox = "gift_box",
    Other = "other",
});
text_enum!(VariantStatus { Active = "active", Archived = "archived" });
text_enum!(NetUnit { G = "g", Kg = "kg", Ml = "ml", L = "l", Piece = "piece" });
text_enum!(CartStatus { Active = "active", Converted = "converted", Abandoned = "abandoned", Expired = "expired" });
text_enum!(FulfillmentMethod {
    Shipping = "shipping",
    Pickup = "pickup",
    StayDelivery = "stay_delivery",
    RelayPoint = "relay_point",
});
text_enum!(DeliveryType { Home = "home", RelayPoint = "relay_point" });
text_enum!(WithdrawalExemption {
    None = "none",
    Perishable = "perishable",
    UnsealedHygiene = "unsealed_hygiene",
    Personalised = "personalised",
});
text_enum!(ShippingRateKind { Flat = "flat", WeightBased = "weight_based", PriceBased = "price_based", Free = "free" });
text_enum!(OrderChannel {
    Website = "website",
    Admin = "admin",
    CampPos = "camp_pos",
    Phone = "phone",
    Whatsapp = "whatsapp",
});
text_enum!(OrderStatus {
    Draft = "draft",
    Pending = "pending",
    Open = "open",
    Completed = "completed",
    Cancelled = "cancelled",
});
text_enum!(OrderPaymentStatus {
    Unpaid = "unpaid",
    Pending = "pending",
    Authorized = "authorized",
    PartiallyPaid = "partially_paid",
    Paid = "paid",
    PartiallyRefunded = "partially_refunded",
    Refunded = "refunded",
    Voided = "voided",
});
text_enum!(OrderFulfillmentStatus {
    Unfulfilled = "unfulfilled",
    PartiallyFulfilled = "partially_fulfilled",
    Fulfilled = "fulfilled",
    PartiallyReturned = "partially_returned",
    Returned = "returned",
});
text_enum!(ShopPaymentMethod {
    Stripe = "stripe",
    CashOnDelivery = "cash_on_delivery",
    PayAtPickup = "pay_at_pickup",
    BankTransfer = "bank_transfer",
});
text_enum!(OrderCancelReason {
    Customer = "customer",
    OutOfStock = "out_of_stock",
    Fraud = "fraud",
    PaymentFailed = "payment_failed",
    Unconfirmed = "unconfirmed",
    Other = "other",
});
text_enum!(RiskLevel { Normal = "normal", Review = "review", High = "high" });
text_enum!(OrderActor { Customer = "customer", Staff = "staff", System = "system", Webhook = "webhook" });
text_enum!(FulfillmentStatus {
    Pending = "pending",
    Packed = "packed",
    ReadyForPickup = "ready_for_pickup",
    Shipped = "shipped",
    InTransit = "in_transit",
    Delivered = "delivered",
    PickedUp = "picked_up",
    Failed = "failed",
    Cancelled = "cancelled",
});
text_enum!(ReturnStatus {
    Requested = "requested",
    Approved = "approved",
    Rejected = "rejected",
    Received = "received",
    Closed = "closed",
});
text_enum!(ReturnReason {
    Damaged = "damaged",
    WrongItem = "wrong_item",
    Quality = "quality",
    NotAsDescribed = "not_as_described",
    ChangedMind = "changed_mind",
    Other = "other",
    Withdrawal = "withdrawal",
    NonConformity = "non_conformity",
});
text_enum!(ReturnCondition { Sellable = "sellable", Damaged = "damaged", Expired = "expired" });
text_enum!(MovementReason {
    Receipt = "receipt",
    Sale = "sale",
    ReturnRestock = "return_restock",
    Adjustment = "adjustment",
    Count = "count",
    Damage = "damage",
    Expired = "expired",
    TransferIn = "transfer_in",
    TransferOut = "transfer_out",
    Correction = "correction",
});
text_enum!(PromotionScope { Booking = "booking", Shop = "shop", All = "all" });
text_enum!(CustomerEligibility { All = "all", FirstOrder = "first_order", StayingGuests = "staying_guests" });

impl FulfillmentStatus {
    /// The parcel left (or was handed over): its items count as fulfilled.
    pub fn counts_as_fulfilled(self) -> bool {
        matches!(
            self,
            Self::Shipped | Self::InTransit | Self::Delivered | Self::PickedUp
        )
    }

    pub fn is_final(self) -> bool {
        matches!(
            self,
            Self::Delivered | Self::PickedUp | Self::Failed | Self::Cancelled
        )
    }
}
