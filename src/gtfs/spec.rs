//! Every file and field of the GTFS Schedule reference
//! (<https://gtfs.org/documentation/schedule/reference/>), as data.
//!
//! One table drives everything that has to know the spec: the editor's generic
//! record engine (which files it stores, their keys, what each field may hold
//! and what it points at), the importer and exporter, the feed report, and the
//! dashboard, which builds its forms from [`spec_json`] (`GET /gtfs-spec`).
//!
//! A file is stored one of two ways ([`Storage`]):
//! - **bespoke**: the tables the editor had before this section - stops,
//!   routes, trips, stop times (as stop orders and timing profiles), calendars,
//!   frequencies - each with change types of its own;
//! - **record**: one table per file, named `gtfs_<entity>`, whose columns are
//!   the file's own field names, edited through the generic `record` changes.
//!
//! Values have one canonical form in the database and the API ([`Canon`]):
//! dates are ISO `YYYY-MM-DD`, times are whole seconds (past 24:00 allowed),
//! amounts are decimal text so a price keeps the digits it was written with.

use serde_json::{json, Map, Value};

/// What a field may hold. The names follow the reference's "Field Types".
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FieldType {
    /// An ID: any text without control characters.
    Id,
    Text,
    Url,
    Email,
    Phone,
    /// An IETF BCP 47 language code.
    Language,
    /// A TZ database name.
    Timezone,
    /// Six hex digits; the editor stores `#RRGGBB`.
    Color,
    /// An ISO 4217 code.
    CurrencyCode,
    /// A decimal amount, kept as written.
    CurrencyAmount,
    /// `YYYYMMDD` in a file, `YYYY-MM-DD` in the API and the database.
    Date,
    /// `H:MM:SS` in a file and the API, whole seconds in the database.
    Time,
    Latitude,
    Longitude,
    Integer {
        min: Option<i64>,
        max: Option<i64>,
    },
    Float {
        min: Option<f64>,
        /// `min` itself is not allowed ("positive").
        exclusive_min: bool,
    },
    /// One of the listed values; each with the label the dashboard shows.
    Enum(&'static [(i64, &'static str)]),
    /// A JSON value (an extension column, a GeoJSON geometry).
    Json,
}

const NON_NEGATIVE_INT: FieldType = FieldType::Integer {
    min: Some(0),
    max: None,
};
const POSITIVE_INT: FieldType = FieldType::Integer {
    min: Some(1),
    max: None,
};
const ANY_INT: FieldType = FieldType::Integer {
    min: None,
    max: None,
};
const NON_NEGATIVE_FLOAT: FieldType = FieldType::Float {
    min: Some(0.0),
    exclusive_min: false,
};
const POSITIVE_FLOAT: FieldType = FieldType::Float {
    min: Some(0.0),
    exclusive_min: true,
};
const ANY_FLOAT: FieldType = FieldType::Float {
    min: None,
    exclusive_min: false,
};

const ZERO_ONE: &[(i64, &str)] = &[(0, "no"), (1, "yes")];
const ACCESSIBILITY: &[(i64, &str)] = &[(0, "no information"), (1, "yes"), (2, "no")];
const PICKUP_DROP_OFF: &[(i64, &str)] = &[
    (0, "regularly scheduled"),
    (1, "none"),
    (2, "phone the agency"),
    (3, "coordinate with the driver"),
];
const CONTINUOUS: &[(i64, &str)] = &[
    (0, "continuous"),
    (1, "none"),
    (2, "phone the agency"),
    (3, "coordinate with the driver"),
];
const LOCATION_TYPES: &[(i64, &str)] = &[
    (0, "stop or platform"),
    (1, "station"),
    (2, "entrance or exit"),
    (3, "generic node"),
    (4, "boarding area"),
];
/// The basic route types, then the extended (Hierarchical Vehicle Type) ones
/// the reference allows; the editor accepts every value 100-1702 besides.
const ROUTE_TYPES: &[(i64, &str)] = &[
    (0, "tram"),
    (1, "subway or metro"),
    (2, "rail"),
    (3, "bus"),
    (4, "ferry"),
    (5, "cable tram"),
    (6, "aerial lift"),
    (7, "funicular"),
    (11, "trolleybus"),
    (12, "monorail"),
];

/// Whether a file has to be in a feed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FilePresence {
    Required,
    Optional,
    /// Required in some feeds; the note says when.
    Conditional(&'static str),
}

/// Whether a field has to be filled.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Presence {
    Required,
    Optional,
    /// Required, or forbidden, depending on other fields; the note says how.
    /// The rule itself is checked by the feed report and the record engine.
    Conditional(&'static str),
}

/// A field that names a row of another file: `file` and the field there.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ref {
    pub file: &'static str,
    pub field: &'static str,
}

const fn r(file: &'static str, field: &'static str) -> Ref {
    Ref { file, field }
}

#[derive(Debug, Clone, Copy)]
pub struct FieldSpec {
    /// The field's name in the file (and, for a record file, the column).
    pub name: &'static str,
    pub ty: FieldType,
    pub presence: Presence,
    /// What it may point at. More than one target means any of them.
    pub refs: &'static [Ref],
    /// Not in the reference: a column a feed of ours carries and GIMS reads
    /// (`stops.info_json`, `feed_info.feed_id`).
    pub extension: bool,
}

const fn f(name: &'static str, ty: FieldType, presence: Presence) -> FieldSpec {
    FieldSpec {
        name,
        ty,
        presence,
        refs: &[],
        extension: false,
    }
}

const fn fr(
    name: &'static str,
    ty: FieldType,
    presence: Presence,
    refs: &'static [Ref],
) -> FieldSpec {
    FieldSpec {
        name,
        ty,
        presence,
        refs,
        extension: false,
    }
}

const fn ext(name: &'static str, ty: FieldType) -> FieldSpec {
    FieldSpec {
        name,
        ty,
        presence: Presence::Optional,
        refs: &[],
        extension: true,
    }
}

/// How a record file's rows are keyed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Key {
    /// One id field is the key (`pathway_id`).
    Field(&'static str),
    /// The file has no id of its own (`transfers.txt`): each row gets a minted
    /// `row_id`, and these fields together must still be unique.
    Minted { natural: &'static [&'static str] },
    /// One row per feed (`feed_info.txt`); the key is the gtfs_id.
    Feed,
}

/// Where a file's rows live.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Storage {
    /// The editor's own tables and change types.
    Bespoke,
    /// `gtfs_<entity>`, edited as `record` changes.
    Record { entity: &'static str, key: Key },
}

#[derive(Debug, Clone, Copy)]
pub struct FileSpec {
    /// `pathways.txt`, or `locations.geojson`.
    pub name: &'static str,
    pub label: &'static str,
    /// Where the dashboard lists it.
    pub group: &'static str,
    pub presence: FilePresence,
    pub storage: Storage,
    pub fields: &'static [FieldSpec],
}

impl FileSpec {
    /// The file's name without its extension: `pathways`.
    pub fn stem(&self) -> &'static str {
        self.name
            .strip_suffix(".txt")
            .or_else(|| self.name.strip_suffix(".geojson"))
            .unwrap_or(self.name)
    }

    pub fn field(&self, name: &str) -> Option<&'static FieldSpec> {
        self.fields.iter().find(|f| f.name == name)
    }

    /// The record entity (`pathway`), if the file is stored as records.
    pub fn entity(&self) -> Option<&'static str> {
        match self.storage {
            Storage::Record { entity, .. } => Some(entity),
            Storage::Bespoke => None,
        }
    }

    pub fn key(&self) -> Option<Key> {
        match self.storage {
            Storage::Record { key, .. } => Some(key),
            Storage::Bespoke => None,
        }
    }

    /// `gtfs_pathway`, for a record file.
    pub fn table(&self) -> Option<String> {
        self.entity().map(|e| format!("gtfs_{e}"))
    }
}

use FieldType::*;
use Presence::{Conditional as Cond, Optional as Opt, Required as Req};

const fn rec(entity: &'static str, key: Key) -> Storage {
    Storage::Record { entity, key }
}

/// The whole reference, in the reference's order.
pub static FILES: &[FileSpec] = &[
    FileSpec {
        name: "agency.txt",
        label: "Agencies",
        group: "Base",
        presence: FilePresence::Required,
        storage: rec("agency", Key::Field("agency_id")),
        fields: &[
            f(
                "agency_id",
                Id,
                Cond("required when the feed has more than one agency"),
            ),
            f("agency_name", Text, Req),
            f("agency_url", Url, Req),
            f("agency_timezone", Timezone, Req),
            f("agency_lang", Language, Opt),
            f("agency_phone", Phone, Opt),
            f("agency_fare_url", Url, Opt),
            f("agency_email", Email, Opt),
        ],
    },
    FileSpec {
        name: "stops.txt",
        label: "Stops, stations and entrances",
        group: "Base",
        presence: FilePresence::Conditional("required unless the feed has only Flex locations"),
        storage: Storage::Bespoke,
        fields: &[
            f("stop_id", Id, Req),
            f("stop_code", Text, Opt),
            f(
                "stop_name",
                Text,
                Cond("required for stops, stations and entrances"),
            ),
            f("tts_stop_name", Text, Opt),
            f("stop_desc", Text, Opt),
            f(
                "stop_lat",
                Latitude,
                Cond("required for stops, stations and entrances"),
            ),
            f(
                "stop_lon",
                Longitude,
                Cond("required for stops, stations and entrances"),
            ),
            fr(
                "zone_id",
                Id,
                Cond("required when fare_rules.txt uses zones"),
                &[],
            ),
            f("stop_url", Url, Opt),
            f("location_type", Enum(LOCATION_TYPES), Opt),
            fr(
                "parent_station",
                Id,
                Cond("required for entrances, generic nodes and boarding areas; forbidden on stations"),
                &[r("stops.txt", "stop_id")],
            ),
            f("stop_timezone", Timezone, Opt),
            f("wheelchair_boarding", Enum(ACCESSIBILITY), Opt),
            fr("level_id", Id, Opt, &[r("levels.txt", "level_id")]),
            f("platform_code", Text, Opt),
            f(
                "stop_access",
                Enum(&[(0, "only through the station"), (1, "straight from the street")]),
                Cond("only on a stop with a parent station"),
            ),
            ext("info_json", Json),
        ],
    },
    FileSpec {
        name: "routes.txt",
        label: "Routes",
        group: "Base",
        presence: FilePresence::Required,
        storage: Storage::Bespoke,
        fields: &[
            f("route_id", Id, Req),
            fr(
                "agency_id",
                Id,
                Cond("required when the feed has more than one agency"),
                &[r("agency.txt", "agency_id")],
            ),
            f(
                "route_short_name",
                Text,
                Cond("a short or a long name is required"),
            ),
            f(
                "route_long_name",
                Text,
                Cond("a short or a long name is required"),
            ),
            f("route_desc", Text, Opt),
            f("route_type", Enum(ROUTE_TYPES), Req),
            f("route_url", Url, Opt),
            f("route_color", Color, Opt),
            f("route_text_color", Color, Opt),
            f("route_sort_order", NON_NEGATIVE_INT, Opt),
            f(
                "continuous_pickup",
                Enum(CONTINUOUS),
                Cond("forbidden when stop_times.txt uses Flex windows"),
            ),
            f(
                "continuous_drop_off",
                Enum(CONTINUOUS),
                Cond("forbidden when stop_times.txt uses Flex windows"),
            ),
            fr(
                "network_id",
                Id,
                Cond("forbidden when route_networks.txt exists"),
                &[],
            ),
        ],
    },
    FileSpec {
        name: "trips.txt",
        label: "Trips",
        group: "Base",
        presence: FilePresence::Required,
        storage: Storage::Bespoke,
        fields: &[
            fr("route_id", Id, Req, &[r("routes.txt", "route_id")]),
            fr(
                "service_id",
                Id,
                Req,
                &[
                    r("calendar.txt", "service_id"),
                    r("calendar_dates.txt", "service_id"),
                ],
            ),
            f("trip_id", Id, Req),
            f("trip_headsign", Text, Opt),
            f("trip_short_name", Text, Opt),
            f(
                "direction_id",
                Enum(&[(0, "one direction"), (1, "the other direction")]),
                Opt,
            ),
            f("block_id", Id, Opt),
            fr(
                "shape_id",
                Id,
                Cond("required when a stop time uses continuous pickup or drop-off"),
                &[r("shapes.txt", "shape_id")],
            ),
            f("wheelchair_accessible", Enum(ACCESSIBILITY), Opt),
            f("bikes_allowed", Enum(ACCESSIBILITY), Opt),
            f("cars_allowed", Enum(ACCESSIBILITY), Opt),
        ],
    },
    FileSpec {
        name: "stop_times.txt",
        label: "Stop times",
        group: "Base",
        presence: FilePresence::Required,
        storage: Storage::Bespoke,
        fields: &[
            fr("trip_id", Id, Req, &[r("trips.txt", "trip_id")]),
            f("arrival_time", Time, Cond("required at the first and last stop and at timepoints")),
            f("departure_time", Time, Cond("required at timepoints")),
            fr(
                "stop_id",
                Id,
                Cond("required unless the row names a Flex location"),
                &[r("stops.txt", "stop_id")],
            ),
            fr(
                "location_group_id",
                Id,
                Cond("Flex: forbidden when stop_id is given"),
                &[r("location_groups.txt", "location_group_id")],
            ),
            fr(
                "location_id",
                Id,
                Cond("Flex: forbidden when stop_id is given"),
                &[r("locations.geojson", "location_id")],
            ),
            f("stop_sequence", NON_NEGATIVE_INT, Req),
            f("stop_headsign", Text, Opt),
            f("start_pickup_drop_off_window", Time, Cond("Flex only")),
            f("end_pickup_drop_off_window", Time, Cond("Flex only")),
            f("pickup_type", Enum(PICKUP_DROP_OFF), Cond("restricted on Flex rows")),
            f("drop_off_type", Enum(PICKUP_DROP_OFF), Cond("restricted on Flex rows")),
            f("continuous_pickup", Enum(CONTINUOUS), Cond("forbidden on Flex rows")),
            f("continuous_drop_off", Enum(CONTINUOUS), Cond("forbidden on Flex rows")),
            f("shape_dist_traveled", NON_NEGATIVE_FLOAT, Opt),
            f(
                "timepoint",
                Enum(&[(0, "approximate"), (1, "exact")]),
                Opt,
            ),
            fr(
                "pickup_booking_rule_id",
                Id,
                Opt,
                &[r("booking_rules.txt", "booking_rule_id")],
            ),
            fr(
                "drop_off_booking_rule_id",
                Id,
                Opt,
                &[r("booking_rules.txt", "booking_rule_id")],
            ),
        ],
    },
    FileSpec {
        name: "calendar.txt",
        label: "Service days",
        group: "Base",
        presence: FilePresence::Conditional("required unless every service is in calendar_dates.txt"),
        storage: Storage::Bespoke,
        fields: &[
            f("service_id", Id, Req),
            f("monday", Enum(ZERO_ONE), Req),
            f("tuesday", Enum(ZERO_ONE), Req),
            f("wednesday", Enum(ZERO_ONE), Req),
            f("thursday", Enum(ZERO_ONE), Req),
            f("friday", Enum(ZERO_ONE), Req),
            f("saturday", Enum(ZERO_ONE), Req),
            f("sunday", Enum(ZERO_ONE), Req),
            f("start_date", Date, Req),
            f("end_date", Date, Req),
        ],
    },
    FileSpec {
        name: "calendar_dates.txt",
        label: "Service exceptions",
        group: "Base",
        presence: FilePresence::Conditional("required unless calendar.txt lists every service day"),
        storage: Storage::Bespoke,
        fields: &[
            f("service_id", Id, Req),
            f("date", Date, Req),
            f(
                "exception_type",
                Enum(&[(1, "service added"), (2, "service removed")]),
                Req,
            ),
        ],
    },
    FileSpec {
        name: "fare_attributes.txt",
        label: "Fares (v1)",
        group: "Fares",
        presence: FilePresence::Optional,
        storage: rec("fare_attribute", Key::Field("fare_id")),
        fields: &[
            f("fare_id", Id, Req),
            f("price", NON_NEGATIVE_FLOAT, Req),
            f("currency_type", CurrencyCode, Req),
            f(
                "payment_method",
                Enum(&[(0, "paid on board"), (1, "paid before boarding")]),
                Req,
            ),
            f(
                "transfers",
                Enum(&[(0, "no transfers"), (1, "one transfer"), (2, "two transfers")]),
                Cond("empty means unlimited transfers"),
            ),
            fr(
                "agency_id",
                Id,
                Cond("required when the feed has more than one agency"),
                &[r("agency.txt", "agency_id")],
            ),
            f("transfer_duration", NON_NEGATIVE_INT, Opt),
        ],
    },
    FileSpec {
        name: "fare_rules.txt",
        label: "Fare rules (v1)",
        group: "Fares",
        presence: FilePresence::Optional,
        storage: rec(
            "fare_rule",
            Key::Minted {
                natural: &["fare_id", "route_id", "origin_id", "destination_id", "contains_id"],
            },
        ),
        fields: &[
            fr("fare_id", Id, Req, &[r("fare_attributes.txt", "fare_id")]),
            fr("route_id", Id, Opt, &[r("routes.txt", "route_id")]),
            fr("origin_id", Id, Opt, &[r("stops.txt", "zone_id")]),
            fr("destination_id", Id, Opt, &[r("stops.txt", "zone_id")]),
            fr("contains_id", Id, Opt, &[r("stops.txt", "zone_id")]),
        ],
    },
    FileSpec {
        name: "timeframes.txt",
        label: "Timeframes",
        group: "Fares",
        presence: FilePresence::Optional,
        storage: rec(
            "timeframe",
            Key::Minted {
                natural: &["timeframe_group_id", "start_time", "end_time", "service_id"],
            },
        ),
        fields: &[
            f("timeframe_group_id", Id, Req),
            f("start_time", Time, Cond("given together with end_time")),
            f("end_time", Time, Cond("given together with start_time")),
            fr(
                "service_id",
                Id,
                Req,
                &[
                    r("calendar.txt", "service_id"),
                    r("calendar_dates.txt", "service_id"),
                ],
            ),
        ],
    },
    FileSpec {
        name: "rider_categories.txt",
        label: "Rider categories",
        group: "Fares",
        presence: FilePresence::Optional,
        storage: rec("rider_category", Key::Field("rider_category_id")),
        fields: &[
            f("rider_category_id", Id, Req),
            f("rider_category_name", Text, Req),
            f("is_default_fare_category", Enum(ZERO_ONE), Req),
            f("eligibility_url", Url, Opt),
        ],
    },
    FileSpec {
        name: "fare_media.txt",
        label: "Fare media",
        group: "Fares",
        presence: FilePresence::Optional,
        storage: rec("fare_media", Key::Field("fare_media_id")),
        fields: &[
            f("fare_media_id", Id, Req),
            f("fare_media_name", Text, Opt),
            f(
                "fare_media_type",
                Enum(&[
                    (0, "none (paper ticket, cash)"),
                    (1, "physical paper ticket"),
                    (2, "physical transit card"),
                    (3, "contactless bank card or device"),
                    (4, "mobile app"),
                ]),
                Req,
            ),
        ],
    },
    FileSpec {
        name: "fare_products.txt",
        label: "Fare products",
        group: "Fares",
        presence: FilePresence::Optional,
        storage: rec(
            "fare_product",
            Key::Minted {
                natural: &["fare_product_id", "rider_category_id", "fare_media_id"],
            },
        ),
        fields: &[
            f("fare_product_id", Id, Req),
            f("fare_product_name", Text, Opt),
            fr(
                "rider_category_id",
                Id,
                Opt,
                &[r("rider_categories.txt", "rider_category_id")],
            ),
            fr(
                "fare_media_id",
                Id,
                Opt,
                &[r("fare_media.txt", "fare_media_id")],
            ),
            f("amount", CurrencyAmount, Req),
            f("currency", CurrencyCode, Req),
        ],
    },
    FileSpec {
        name: "fare_leg_rules.txt",
        label: "Fare leg rules",
        group: "Fares",
        presence: FilePresence::Optional,
        storage: rec(
            "fare_leg_rule",
            Key::Minted {
                natural: &[
                    "network_id",
                    "from_area_id",
                    "to_area_id",
                    "from_timeframe_group_id",
                    "to_timeframe_group_id",
                    "fare_product_id",
                ],
            },
        ),
        fields: &[
            f("leg_group_id", Id, Opt),
            fr(
                "network_id",
                Id,
                Opt,
                &[r("routes.txt", "network_id"), r("networks.txt", "network_id")],
            ),
            fr("from_area_id", Id, Opt, &[r("areas.txt", "area_id")]),
            fr("to_area_id", Id, Opt, &[r("areas.txt", "area_id")]),
            fr(
                "from_timeframe_group_id",
                Id,
                Opt,
                &[r("timeframes.txt", "timeframe_group_id")],
            ),
            fr(
                "to_timeframe_group_id",
                Id,
                Opt,
                &[r("timeframes.txt", "timeframe_group_id")],
            ),
            fr(
                "fare_product_id",
                Id,
                Req,
                &[r("fare_products.txt", "fare_product_id")],
            ),
            f("rule_priority", NON_NEGATIVE_INT, Opt),
        ],
    },
    FileSpec {
        name: "fare_leg_join_rules.txt",
        label: "Fare leg join rules",
        group: "Fares",
        presence: FilePresence::Optional,
        storage: rec(
            "fare_leg_join_rule",
            Key::Minted {
                natural: &["from_network_id", "to_network_id", "from_stop_id", "to_stop_id"],
            },
        ),
        fields: &[
            fr(
                "from_network_id",
                Id,
                Req,
                &[r("routes.txt", "network_id"), r("networks.txt", "network_id")],
            ),
            fr(
                "to_network_id",
                Id,
                Req,
                &[r("routes.txt", "network_id"), r("networks.txt", "network_id")],
            ),
            fr(
                "from_stop_id",
                Id,
                Cond("given together with to_stop_id"),
                &[r("stops.txt", "stop_id")],
            ),
            fr(
                "to_stop_id",
                Id,
                Cond("given together with from_stop_id"),
                &[r("stops.txt", "stop_id")],
            ),
        ],
    },
    FileSpec {
        name: "fare_transfer_rules.txt",
        label: "Fare transfer rules",
        group: "Fares",
        presence: FilePresence::Optional,
        storage: rec(
            "fare_transfer_rule",
            Key::Minted {
                natural: &[
                    "from_leg_group_id",
                    "to_leg_group_id",
                    "fare_product_id",
                    "transfer_count",
                    "duration_limit",
                ],
            },
        ),
        fields: &[
            fr(
                "from_leg_group_id",
                Id,
                Opt,
                &[r("fare_leg_rules.txt", "leg_group_id")],
            ),
            fr(
                "to_leg_group_id",
                Id,
                Opt,
                &[r("fare_leg_rules.txt", "leg_group_id")],
            ),
            f(
                "transfer_count",
                FieldType::Integer {
                    min: Some(-1),
                    max: None,
                },
                Cond("required when both leg groups are the same; -1 means unlimited"),
            ),
            f("duration_limit", POSITIVE_INT, Opt),
            f(
                "duration_limit_type",
                Enum(&[
                    (0, "departure of the first leg to arrival of the second"),
                    (1, "departure to departure"),
                    (2, "arrival to departure"),
                    (3, "arrival to arrival"),
                ]),
                Cond("required when duration_limit is given"),
            ),
            f(
                "fare_transfer_type",
                Enum(&[
                    (0, "first leg + transfer"),
                    (1, "first leg + transfer + second leg"),
                    (2, "transfer only"),
                ]),
                Req,
            ),
            fr(
                "fare_product_id",
                Id,
                Opt,
                &[r("fare_products.txt", "fare_product_id")],
            ),
        ],
    },
    FileSpec {
        name: "areas.txt",
        label: "Areas",
        group: "Fares",
        presence: FilePresence::Optional,
        storage: rec("area", Key::Field("area_id")),
        fields: &[f("area_id", Id, Req), f("area_name", Text, Opt)],
    },
    FileSpec {
        name: "stop_areas.txt",
        label: "Stops in areas",
        group: "Fares",
        presence: FilePresence::Optional,
        storage: rec(
            "stop_area",
            Key::Minted {
                natural: &["area_id", "stop_id"],
            },
        ),
        fields: &[
            fr("area_id", Id, Req, &[r("areas.txt", "area_id")]),
            fr("stop_id", Id, Req, &[r("stops.txt", "stop_id")]),
        ],
    },
    FileSpec {
        name: "networks.txt",
        label: "Networks",
        group: "Fares",
        presence: FilePresence::Conditional("forbidden when routes.txt has network_id"),
        storage: rec("network", Key::Field("network_id")),
        fields: &[f("network_id", Id, Req), f("network_name", Text, Opt)],
    },
    FileSpec {
        name: "route_networks.txt",
        label: "Routes in networks",
        group: "Fares",
        presence: FilePresence::Conditional("forbidden when routes.txt has network_id"),
        storage: rec(
            "route_network",
            Key::Minted {
                natural: &["route_id"],
            },
        ),
        fields: &[
            fr("network_id", Id, Req, &[r("networks.txt", "network_id")]),
            fr("route_id", Id, Req, &[r("routes.txt", "route_id")]),
        ],
    },
    FileSpec {
        name: "shapes.txt",
        label: "Shapes",
        group: "Base",
        presence: FilePresence::Optional,
        storage: rec("shape", Key::Field("shape_id")),
        fields: &[
            f("shape_id", Id, Req),
            f("shape_pt_lat", Latitude, Req),
            f("shape_pt_lon", Longitude, Req),
            f("shape_pt_sequence", NON_NEGATIVE_INT, Req),
            f("shape_dist_traveled", NON_NEGATIVE_FLOAT, Opt),
        ],
    },
    FileSpec {
        name: "frequencies.txt",
        label: "Headways",
        group: "Base",
        presence: FilePresence::Optional,
        storage: Storage::Bespoke,
        fields: &[
            fr("trip_id", Id, Req, &[r("trips.txt", "trip_id")]),
            f("start_time", Time, Req),
            f("end_time", Time, Req),
            f("headway_secs", POSITIVE_INT, Req),
            f(
                "exact_times",
                Enum(&[(0, "frequency-based"), (1, "schedule-based")]),
                Opt,
            ),
        ],
    },
    FileSpec {
        name: "transfers.txt",
        label: "Transfers",
        group: "Stations",
        presence: FilePresence::Optional,
        storage: rec(
            "transfer",
            Key::Minted {
                natural: &[
                    "from_stop_id",
                    "to_stop_id",
                    "from_trip_id",
                    "to_trip_id",
                    "from_route_id",
                    "to_route_id",
                ],
            },
        ),
        fields: &[
            fr(
                "from_stop_id",
                Id,
                Cond("required for transfer types 1-3"),
                &[r("stops.txt", "stop_id")],
            ),
            fr(
                "to_stop_id",
                Id,
                Cond("required for transfer types 1-3"),
                &[r("stops.txt", "stop_id")],
            ),
            fr("from_route_id", Id, Opt, &[r("routes.txt", "route_id")]),
            fr("to_route_id", Id, Opt, &[r("routes.txt", "route_id")]),
            fr(
                "from_trip_id",
                Id,
                Cond("required for transfer types 4 and 5"),
                &[r("trips.txt", "trip_id")],
            ),
            fr(
                "to_trip_id",
                Id,
                Cond("required for transfer types 4 and 5"),
                &[r("trips.txt", "trip_id")],
            ),
            f(
                "transfer_type",
                Enum(&[
                    (0, "recommended"),
                    (1, "timed"),
                    (2, "needs a minimum time"),
                    (3, "not possible"),
                    (4, "in-seat"),
                    (5, "re-board, no in-seat"),
                ]),
                Req,
            ),
            f("min_transfer_time", NON_NEGATIVE_INT, Opt),
        ],
    },
    FileSpec {
        name: "pathways.txt",
        label: "Pathways",
        group: "Stations",
        presence: FilePresence::Optional,
        storage: rec("pathway", Key::Field("pathway_id")),
        fields: &[
            f("pathway_id", Id, Req),
            fr("from_stop_id", Id, Req, &[r("stops.txt", "stop_id")]),
            fr("to_stop_id", Id, Req, &[r("stops.txt", "stop_id")]),
            f(
                "pathway_mode",
                Enum(&[
                    (1, "walkway"),
                    (2, "stairs"),
                    (3, "moving sidewalk"),
                    (4, "escalator"),
                    (5, "elevator"),
                    (6, "fare gate"),
                    (7, "exit gate"),
                ]),
                Req,
            ),
            f(
                "is_bidirectional",
                Enum(&[(0, "one way"), (1, "both ways")]),
                Req,
            ),
            f("length", NON_NEGATIVE_FLOAT, Opt),
            f("traversal_time", POSITIVE_INT, Opt),
            f("stair_count", ANY_INT, Opt),
            f("max_slope", ANY_FLOAT, Opt),
            f("min_width", POSITIVE_FLOAT, Opt),
            f("signposted_as", Text, Opt),
            f("reversed_signposted_as", Text, Opt),
        ],
    },
    FileSpec {
        name: "levels.txt",
        label: "Levels",
        group: "Stations",
        presence: FilePresence::Conditional("required when elevator pathways exist"),
        storage: rec("level", Key::Field("level_id")),
        fields: &[
            f("level_id", Id, Req),
            f("level_index", ANY_FLOAT, Req),
            f("level_name", Text, Opt),
        ],
    },
    FileSpec {
        name: "location_groups.txt",
        label: "Location groups",
        group: "Flex",
        presence: FilePresence::Optional,
        storage: rec("location_group", Key::Field("location_group_id")),
        fields: &[
            f("location_group_id", Id, Req),
            f("location_group_name", Text, Opt),
        ],
    },
    FileSpec {
        name: "location_group_stops.txt",
        label: "Stops in location groups",
        group: "Flex",
        presence: FilePresence::Optional,
        storage: rec(
            "location_group_stop",
            Key::Minted {
                natural: &["location_group_id", "stop_id"],
            },
        ),
        fields: &[
            fr(
                "location_group_id",
                Id,
                Req,
                &[r("location_groups.txt", "location_group_id")],
            ),
            fr("stop_id", Id, Req, &[r("stops.txt", "stop_id")]),
        ],
    },
    FileSpec {
        name: "locations.geojson",
        label: "Flex zones",
        group: "Flex",
        presence: FilePresence::Optional,
        storage: rec("location", Key::Field("location_id")),
        fields: &[
            f("location_id", Id, Req),
            f("stop_name", Text, Opt),
            f("stop_desc", Text, Opt),
            f("geometry", Json, Req),
        ],
    },
    FileSpec {
        name: "booking_rules.txt",
        label: "Booking rules",
        group: "Flex",
        presence: FilePresence::Optional,
        storage: rec("booking_rule", Key::Field("booking_rule_id")),
        fields: &[
            f("booking_rule_id", Id, Req),
            f(
                "booking_type",
                Enum(&[
                    (0, "real time"),
                    (1, "same day, with notice"),
                    (2, "up to a day or more before"),
                ]),
                Req,
            ),
            f(
                "prior_notice_duration_min",
                ANY_INT,
                Cond("required for same-day booking"),
            ),
            f(
                "prior_notice_duration_max",
                ANY_INT,
                Cond("same-day booking only"),
            ),
            f(
                "prior_notice_last_day",
                ANY_INT,
                Cond("required for booking a day or more before"),
            ),
            f(
                "prior_notice_last_time",
                Time,
                Cond("required with prior_notice_last_day"),
            ),
            f("prior_notice_start_day", ANY_INT, Cond("not for real-time booking")),
            f(
                "prior_notice_start_time",
                Time,
                Cond("required with prior_notice_start_day"),
            ),
            fr(
                "prior_notice_service_id",
                Id,
                Cond("booking a day or more before only"),
                &[
                    r("calendar.txt", "service_id"),
                    r("calendar_dates.txt", "service_id"),
                ],
            ),
            f("message", Text, Opt),
            f("pickup_message", Text, Opt),
            f("drop_off_message", Text, Opt),
            f("phone_number", Phone, Opt),
            f("info_url", Url, Opt),
            f("booking_url", Url, Opt),
        ],
    },
    FileSpec {
        name: "translations.txt",
        label: "Translations",
        group: "Other",
        presence: FilePresence::Optional,
        storage: rec(
            "translation",
            Key::Minted {
                natural: &[
                    "table_name",
                    "field_name",
                    "language",
                    "record_id",
                    "record_sub_id",
                    "field_value",
                ],
            },
        ),
        fields: &[
            f("table_name", Text, Req),
            f("field_name", Text, Req),
            f("language", Language, Req),
            f("translation", Text, Req),
            f(
                "record_id",
                Text,
                Cond("forbidden for feed_info; required unless field_value is given"),
            ),
            f("record_sub_id", Text, Cond("stop_times only")),
            f("field_value", Text, Cond("required unless record_id is given")),
        ],
    },
    FileSpec {
        name: "feed_info.txt",
        label: "Feed information",
        group: "Other",
        presence: FilePresence::Conditional("required when translations.txt exists"),
        storage: rec("feed_info", Key::Feed),
        fields: &[
            f("feed_publisher_name", Text, Req),
            f("feed_publisher_url", Url, Req),
            f("feed_lang", Language, Req),
            f("default_lang", Language, Opt),
            f("feed_start_date", Date, Opt),
            f("feed_end_date", Date, Opt),
            f("feed_version", Text, Opt),
            f("feed_contact_email", Email, Opt),
            f("feed_contact_url", Url, Opt),
            ext("feed_id", Id),
        ],
    },
    FileSpec {
        name: "attributions.txt",
        label: "Attributions",
        group: "Other",
        presence: FilePresence::Optional,
        storage: rec(
            "attribution",
            Key::Minted {
                natural: &[
                    "attribution_id",
                    "agency_id",
                    "route_id",
                    "trip_id",
                    "organization_name",
                ],
            },
        ),
        fields: &[
            f("attribution_id", Id, Opt),
            fr("agency_id", Id, Opt, &[r("agency.txt", "agency_id")]),
            fr("route_id", Id, Opt, &[r("routes.txt", "route_id")]),
            fr("trip_id", Id, Opt, &[r("trips.txt", "trip_id")]),
            f("organization_name", Text, Req),
            f("is_producer", Enum(ZERO_ONE), Cond("one of the three roles is 1")),
            f("is_operator", Enum(ZERO_ONE), Cond("one of the three roles is 1")),
            f("is_authority", Enum(ZERO_ONE), Cond("one of the three roles is 1")),
            f("attribution_url", Url, Opt),
            f("attribution_email", Email, Opt),
            f("attribution_phone", Phone, Opt),
        ],
    },
];

/// A file by its name (`pathways.txt`) or stem (`pathways`).
pub fn file(name: &str) -> Option<&'static FileSpec> {
    FILES.iter().find(|f| f.name == name || f.stem() == name)
}

/// The record file whose entity this is (`pathway` -> pathways.txt).
pub fn record_file(entity: &str) -> Option<&'static FileSpec> {
    FILES.iter().find(|f| f.entity() == Some(entity))
}

/// Every record entity, in the reference's order.
pub fn record_entities() -> impl Iterator<Item = &'static str> {
    FILES.iter().filter_map(FileSpec::entity)
}

/// The change entities the editor had before the record engine: each has
/// change types of its own. Together with [`record_entities`] this is every
/// value `gtfs_change.entity` may hold.
pub const BESPOKE_ENTITIES: &[&str] = &[
    "stop",
    "route",
    "route_stops",
    "station",
    "feed_config",
    "pattern",
    "timing_profile",
    "route_trips",
    "service",
];

/// Every field, in any file, that points at `file.field` - who would be left
/// pointing at nothing if that row went.
pub fn references_to(file: &str, field: &str) -> Vec<(&'static FileSpec, &'static FieldSpec)> {
    let mut out = Vec::new();
    for spec in FILES {
        for fs in spec.fields {
            if fs.refs.iter().any(|t| t.file == file && t.field == field) {
                out.push((spec, fs));
            }
        }
    }
    out
}

/// The column type a record table gives a field of this type.
pub fn sql_type(ty: FieldType) -> &'static str {
    match ty {
        Id => "text COLLATE \"C\"",
        Text | Url | Email | Phone | Language | Timezone | Color | CurrencyCode
        | CurrencyAmount => "text",
        Date => "date",
        Time | Integer { .. } | Enum(_) => "integer",
        Latitude | Longitude | Float { .. } => "double precision",
        Json => "jsonb",
    }
}

// ---------------------------------------------------------------- values

/// A value in its canonical form: what the database holds and the API gives
/// out. `None` is an empty cell.
pub type Canon = Option<Value>;

/// Seconds since midnight from `H:MM:SS` / `HH:MM:SS` (hours up to 47, as a
/// service day that runs past midnight needs).
pub fn parse_time(s: &str) -> Option<i64> {
    let mut it = s.trim().split(':');
    let (h, m, sec) = (it.next()?, it.next()?, it.next()?);
    if it.next().is_some() || h.is_empty() || h.len() > 2 || m.len() != 2 || sec.len() != 2 {
        return None;
    }
    let all_digits = |x: &str| x.bytes().all(|b| b.is_ascii_digit());
    if !(all_digits(h) && all_digits(m) && all_digits(sec)) {
        return None;
    }
    let (h, m, sec): (i64, i64, i64) = (h.parse().ok()?, m.parse().ok()?, sec.parse().ok()?);
    if h > 47 || m > 59 || sec > 59 {
        return None;
    }
    Some(h * 3600 + m * 60 + sec)
}

/// `HH:MM:SS`, as GTFS writes a time.
pub fn format_time(secs: i64) -> String {
    format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        (secs / 60) % 60,
        secs % 60
    )
}

/// A calendar date from `YYYYMMDD` (a file) or `YYYY-MM-DD` (the API), as ISO.
pub fn parse_date(s: &str) -> Option<String> {
    let s = s.trim();
    let digits: String = match s.len() {
        8 => s.to_string(),
        10 if s.as_bytes()[4] == b'-' && s.as_bytes()[7] == b'-' => s.replace('-', ""),
        _ => return None,
    };
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let iso = format!("{}-{}-{}", &digits[..4], &digits[4..6], &digits[6..]);
    chrono::NaiveDate::parse_from_str(&iso, "%Y-%m-%d").ok()?;
    Some(iso)
}

/// `YYYYMMDD` from an ISO date.
pub fn format_date(iso: &str) -> String {
    iso.replace('-', "")
}

fn is_decimal(s: &str) -> bool {
    let s = s.strip_prefix('-').unwrap_or(s);
    let mut parts = s.splitn(2, '.');
    let whole = parts.next().unwrap_or("");
    let frac = parts.next();
    !whole.is_empty()
        && whole.bytes().all(|b| b.is_ascii_digit())
        && frac.is_none_or(|f| !f.is_empty() && f.bytes().all(|b| b.is_ascii_digit()))
}

fn number_in(ty: FieldType, n: f64) -> Result<(), String> {
    match ty {
        Integer { min, max } => {
            if n.fract() != 0.0 {
                return Err("must be a whole number".into());
            }
            let n = n as i64;
            if min.is_some_and(|m| n < m) {
                return Err(format!("must be {} or more", min.unwrap()));
            }
            if max.is_some_and(|m| n > m) {
                return Err(format!("must be {} or less", max.unwrap()));
            }
            Ok(())
        }
        Float { min, exclusive_min } => match min {
            Some(m) if exclusive_min && n <= m => Err(format!("must be more than {m}")),
            Some(m) if n < m => Err(format!("must be {m} or more")),
            _ => Ok(()),
        },
        Latitude if !(-90.0..=90.0).contains(&n) => Err("must be between -90 and 90".into()),
        Longitude if !(-180.0..=180.0).contains(&n) => Err("must be between -180 and 180".into()),
        _ => Ok(()),
    }
}

/// A route type the reference allows: a basic one, or an extended one.
pub fn route_type_ok(n: i64) -> bool {
    ROUTE_TYPES.iter().any(|(v, _)| *v == n) || (100..=1702).contains(&n)
}

fn enum_ok(values: &[(i64, &str)], n: i64, is_route_type: bool) -> bool {
    if is_route_type {
        route_type_ok(n)
    } else {
        values.iter().any(|(v, _)| *v == n)
    }
}

/// The canonical form of one text cell of a GTFS file, or why it is not a
/// value of the field's type. Blank is an empty cell.
pub fn from_text(field: &FieldSpec, raw: &str) -> Result<Canon, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Ok(None);
    }
    canon_str(field, s).map(Some)
}

fn canon_str(field: &FieldSpec, s: &str) -> Result<Value, String> {
    let ty = field.ty;
    let is_route_type = field.name == "route_type";
    match ty {
        Id => {
            if s.chars().any(|c| c.is_control()) {
                return Err("must not hold control characters".into());
            }
            Ok(json!(s))
        }
        // text may hold a tab or a line break (a quoted cell), nothing else
        // unprintable
        Text | Phone => {
            if s.chars()
                .any(|c| c.is_control() && !matches!(c, '\t' | '\n' | '\r'))
            {
                return Err("must not hold control characters".into());
            }
            Ok(json!(s))
        }
        Url => {
            if s.starts_with("http://") || s.starts_with("https://") {
                Ok(json!(s))
            } else {
                Err("must be a URL starting http:// or https://".into())
            }
        }
        Email => {
            let ok = s.contains('@') && !s.chars().any(char::is_whitespace);
            if ok {
                Ok(json!(s))
            } else {
                Err("must be an email address".into())
            }
        }
        Language => {
            let mut parts = s.split('-');
            let first = parts.next().unwrap_or("");
            let ok = (2..=3).contains(&first.len())
                && first.bytes().all(|b| b.is_ascii_alphabetic())
                && parts.all(|p| {
                    (1..=8).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_alphanumeric())
                });
            if ok {
                Ok(json!(s))
            } else {
                Err("must be a language code such as en or ta-IN".into())
            }
        }
        Timezone => {
            let ok = s == "UTC"
                || (s.contains('/')
                    && s.split('/').all(|p| {
                        !p.is_empty()
                            && p.chars()
                                .all(|c| c.is_ascii_alphanumeric() || "_+-".contains(c))
                    }));
            if ok {
                Ok(json!(s))
            } else {
                Err("must be a time zone such as Asia/Kolkata".into())
            }
        }
        Color => {
            let hex = s.strip_prefix('#').unwrap_or(s);
            if hex.len() == 6 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                Ok(json!(format!("#{}", hex.to_ascii_uppercase())))
            } else {
                Err("must be a colour of six hex digits".into())
            }
        }
        CurrencyCode => {
            if s.len() == 3 && s.bytes().all(|b| b.is_ascii_uppercase()) {
                Ok(json!(s))
            } else {
                Err("must be a three-letter currency code such as INR".into())
            }
        }
        CurrencyAmount => {
            if is_decimal(s) {
                Ok(json!(s))
            } else {
                Err("must be an amount such as 12.50".into())
            }
        }
        Date => parse_date(s)
            .map(|d| json!(d))
            .ok_or_else(|| "must be a date, YYYYMMDD".to_string()),
        Time => parse_time(s)
            .map(|t| json!(t))
            .ok_or_else(|| "must be a time, H:MM:SS".to_string()),
        Latitude | Longitude | Float { .. } => {
            let n: f64 = s.parse().map_err(|_| "must be a number".to_string())?;
            if !n.is_finite() {
                return Err("must be a number".into());
            }
            number_in(ty, n)?;
            Ok(json!(n))
        }
        Integer { .. } => {
            let n: i64 = s
                .parse()
                .map_err(|_| "must be a whole number".to_string())?;
            number_in(ty, n as f64)?;
            Ok(json!(n))
        }
        Enum(values) => {
            let n: i64 = s.parse().map_err(|_| enum_message(values, is_route_type))?;
            if enum_ok(values, n, is_route_type) {
                Ok(json!(n))
            } else {
                Err(enum_message(values, is_route_type))
            }
        }
        Json => serde_json::from_str(s).map_err(|_| "must be JSON".to_string()),
    }
}

fn enum_message(values: &[(i64, &str)], is_route_type: bool) -> String {
    let list: Vec<String> = values.iter().map(|(v, _)| v.to_string()).collect();
    if is_route_type {
        format!("must be one of {} or 100-1702", list.join(", "))
    } else {
        format!("must be one of {}", list.join(", "))
    }
}

/// The canonical form of a value the API was sent: text as a file would have
/// it, or a JSON number for a number, `null` or `""` for an empty cell. Dates
/// may be ISO or `YYYYMMDD`, times `H:MM:SS` or whole seconds.
pub fn from_api(field: &FieldSpec, v: &Value) -> Result<Canon, String> {
    match (field.ty, v) {
        (_, Value::Null) => Ok(None),
        (Json, v) => Ok(Some(v.clone())),
        (_, Value::String(s)) => from_text(field, s),
        (Time, Value::Number(n)) => {
            let secs = n.as_i64().ok_or("must be a time, H:MM:SS")?;
            if (0..48 * 3600).contains(&secs) {
                Ok(Some(json!(secs)))
            } else {
                Err("must be a time between 00:00:00 and 47:59:59".into())
            }
        }
        (Latitude | Longitude | Float { .. } | Integer { .. } | Enum(_), Value::Number(n)) => {
            from_text(field, &n.to_string())
        }
        (CurrencyAmount | Id | Text, Value::Number(n)) => from_text(field, &n.to_string()),
        (Enum(_), Value::Bool(b)) => from_text(field, if *b { "1" } else { "0" }),
        _ => Err("has the wrong type".into()),
    }
}

/// What the API gives out for a canonical value: times as `HH:MM:SS`,
/// everything else as stored.
pub fn to_api(field: &FieldSpec, v: &Canon) -> Value {
    match (field.ty, v) {
        (_, None) => Value::Null,
        (Time, Some(Value::Number(n))) => json!(format_time(n.as_i64().unwrap_or(0))),
        (_, Some(v)) => v.clone(),
    }
}

/// A canonical value as a GTFS file writes it.
pub fn to_text(field: &FieldSpec, v: &Canon) -> String {
    match (field.ty, v) {
        (_, None) | (_, Some(Value::Null)) => String::new(),
        (Time, Some(Value::Number(n))) => format_time(n.as_i64().unwrap_or(0)),
        (Date, Some(Value::String(s))) => format_date(s),
        (Color, Some(Value::String(s))) => s.trim_start_matches('#').to_string(),
        (Json, Some(v)) => v.to_string(),
        (_, Some(Value::String(s))) => s.clone(),
        (_, Some(Value::Number(n))) => format_number(n),
        (_, Some(v)) => v.to_string(),
    }
}

/// A number as GTFS files usually write it: integers without a fraction.
pub fn format_number(n: &serde_json::Number) -> String {
    if let Some(i) = n.as_i64() {
        return i.to_string();
    }
    match n.as_f64() {
        Some(x) if x.fract() == 0.0 && x.abs() < 1e15 => format!("{}", x as i64),
        Some(x) => x.to_string(),
        None => n.to_string(),
    }
}

// ---------------------------------------------------------------- the API's view

fn type_json(ty: FieldType, is_route_type: bool) -> Value {
    match ty {
        Integer { min, max } => json!({"type": "integer", "min": min, "max": max}),
        Float { min, exclusive_min } => {
            json!({"type": "float", "min": min, "exclusive_min": exclusive_min})
        }
        Enum(values) => json!({
            "type": "enum",
            "values": values.iter().map(|(v, l)| json!({"value": v, "label": l})).collect::<Vec<_>>(),
            "extended_route_types": is_route_type,
        }),
        other => json!({"type": match other {
            Id => "id",
            Text => "text",
            Url => "url",
            Email => "email",
            Phone => "phone",
            Language => "language",
            Timezone => "timezone",
            Color => "color",
            CurrencyCode => "currency_code",
            CurrencyAmount => "currency_amount",
            Date => "date",
            Time => "time",
            Latitude => "latitude",
            Longitude => "longitude",
            Json => "json",
            _ => "text",
        }}),
    }
}

fn presence_json(p: Presence) -> Value {
    match p {
        Presence::Required => json!({"presence": "required"}),
        Presence::Optional => json!({"presence": "optional"}),
        Presence::Conditional(note) => json!({"presence": "conditional", "note": note}),
    }
}

/// The whole registry, for `GET /gtfs-spec`: what the dashboard builds its
/// file list, grids and forms from.
pub fn spec_json() -> Value {
    let files: Vec<Value> = FILES
        .iter()
        .map(|spec| {
            let fields: Vec<Value> = spec
                .fields
                .iter()
                .map(|fs| {
                    let mut o = Map::new();
                    o.insert("name".into(), json!(fs.name));
                    o.extend(
                        type_json(fs.ty, fs.name == "route_type")
                            .as_object()
                            .cloned()
                            .unwrap_or_default(),
                    );
                    o.extend(
                        presence_json(fs.presence)
                            .as_object()
                            .cloned()
                            .unwrap_or_default(),
                    );
                    o.insert(
                        "refs".into(),
                        json!(fs
                            .refs
                            .iter()
                            .map(|t| json!({"file": t.file, "field": t.field}))
                            .collect::<Vec<_>>()),
                    );
                    o.insert("extension".into(), json!(fs.extension));
                    Value::Object(o)
                })
                .collect();
            let (storage, entity, key) = match spec.storage {
                Storage::Bespoke => ("bespoke", Value::Null, Value::Null),
                Storage::Record { entity, key } => (
                    "record",
                    json!(entity),
                    match key {
                        Key::Field(f) => json!({"kind": "field", "field": f}),
                        Key::Minted { natural } => json!({"kind": "minted", "natural": natural}),
                        Key::Feed => json!({"kind": "feed"}),
                    },
                ),
            };
            let (presence, note) = match spec.presence {
                FilePresence::Required => ("required", Value::Null),
                FilePresence::Optional => ("optional", Value::Null),
                FilePresence::Conditional(n) => ("conditional", json!(n)),
            };
            json!({
                "file": spec.name,
                "stem": spec.stem(),
                "label": spec.label,
                "group": spec.group,
                "presence": presence,
                "presence_note": note,
                "storage": storage,
                "entity": entity,
                "key": key,
                "fields": fields,
            })
        })
        .collect();
    json!({"version": "gtfs-schedule-2025", "files": files})
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn every_file_and_field_is_named_once() {
        let mut names = HashSet::new();
        for spec in FILES {
            assert!(names.insert(spec.name), "{} twice", spec.name);
            let mut fields = HashSet::new();
            for fs in spec.fields {
                assert!(fields.insert(fs.name), "{}.{} twice", spec.name, fs.name);
            }
        }
        // the reference's 32 files: 31 .txt and the Flex GeoJSON
        assert_eq!(FILES.len(), 32);
    }

    #[test]
    fn every_reference_points_at_a_field_that_exists() {
        for spec in FILES {
            for fs in spec.fields {
                for t in fs.refs {
                    let target = file(t.file).unwrap_or_else(|| {
                        panic!("{}.{} -> no file {}", spec.name, fs.name, t.file)
                    });
                    assert!(
                        target.field(t.field).is_some(),
                        "{}.{} -> {}.{} does not exist",
                        spec.name,
                        fs.name,
                        t.file,
                        t.field
                    );
                }
            }
        }
    }

    #[test]
    fn every_record_key_is_a_field_of_its_file() {
        for spec in FILES {
            match spec.key() {
                Some(Key::Field(k)) => assert!(spec.field(k).is_some(), "{} key {k}", spec.name),
                Some(Key::Minted { natural }) => {
                    for k in natural {
                        assert!(spec.field(k).is_some(), "{} natural {k}", spec.name)
                    }
                }
                Some(Key::Feed) | None => {}
            }
        }
    }

    #[test]
    fn record_entities_and_bespoke_entities_do_not_overlap() {
        let records: HashSet<&str> = record_entities().collect();
        for b in BESPOKE_ENTITIES {
            assert!(!records.contains(b), "{b} is both");
        }
        assert_eq!(records.len(), record_entities().count(), "an entity twice");
        assert!(records.contains("pathway") && records.contains("feed_info"));
        assert_eq!(
            record_file("fare_attribute").unwrap().name,
            "fare_attributes.txt"
        );
    }

    #[test]
    fn a_stop_is_pointed_at_by_everything_that_names_one() {
        let from: HashSet<String> = references_to("stops.txt", "stop_id")
            .iter()
            .map(|(s, f)| format!("{}.{}", s.stem(), f.name))
            .collect();
        for want in [
            "stops.parent_station",
            "stop_times.stop_id",
            "pathways.from_stop_id",
            "pathways.to_stop_id",
            "transfers.from_stop_id",
            "stop_areas.stop_id",
            "location_group_stops.stop_id",
            "fare_leg_join_rules.from_stop_id",
        ] {
            assert!(from.contains(want), "{want} missing from {from:?}");
        }
    }

    #[test]
    fn times_run_past_midnight_and_read_back_as_written() {
        assert_eq!(parse_time("5:30:00"), Some(19800));
        assert_eq!(parse_time("05:30:00"), Some(19800));
        assert_eq!(parse_time("26:07:15"), Some(26 * 3600 + 7 * 60 + 15));
        assert_eq!(format_time(26 * 3600 + 7 * 60 + 15), "26:07:15");
        for bad in [
            "",
            "5:30",
            "05:60:00",
            "48:00:00",
            "a:00:00",
            "5:3:00",
            "123:00:00",
        ] {
            assert_eq!(parse_time(bad), None, "{bad}");
        }
    }

    #[test]
    fn dates_are_read_either_way_and_kept_as_iso() {
        assert_eq!(parse_date("20260923").as_deref(), Some("2026-09-23"));
        assert_eq!(parse_date("2026-09-23").as_deref(), Some("2026-09-23"));
        assert_eq!(parse_date("20260230"), None);
        assert_eq!(parse_date("2026923"), None);
        assert_eq!(format_date("2026-09-23"), "20260923");
    }

    fn field_of(file_name: &str, name: &str) -> &'static FieldSpec {
        file(file_name).unwrap().field(name).unwrap()
    }

    #[test]
    fn a_cell_becomes_its_canonical_value_or_says_why_not() {
        let mode = field_of("pathways.txt", "pathway_mode");
        assert_eq!(from_text(mode, "4").unwrap(), Some(json!(4)));
        assert!(from_text(mode, "8").unwrap_err().contains("1, 2, 3"));
        assert_eq!(from_text(mode, "  ").unwrap(), None);

        let rt = field_of("routes.txt", "route_type");
        assert_eq!(from_text(rt, "3").unwrap(), Some(json!(3)));
        assert_eq!(from_text(rt, "109").unwrap(), Some(json!(109)));
        assert!(from_text(rt, "8").is_err());

        let color = field_of("routes.txt", "route_color");
        assert_eq!(from_text(color, "ff0000").unwrap(), Some(json!("#FF0000")));
        assert_eq!(to_text(color, &Some(json!("#FF0000"))), "FF0000");

        let price = field_of("fare_products.txt", "amount");
        assert_eq!(from_text(price, "12.50").unwrap(), Some(json!("12.50")));
        assert!(from_text(price, "12.").is_err());

        let width = field_of("pathways.txt", "min_width");
        assert!(from_text(width, "0").is_err());
        assert_eq!(from_text(width, "1.5").unwrap(), Some(json!(1.5)));

        let tc = field_of("fare_transfer_rules.txt", "transfer_count");
        assert_eq!(from_text(tc, "-1").unwrap(), Some(json!(-1)));
        assert!(from_text(tc, "-2").is_err());

        assert!(from_text(field_of("agency.txt", "agency_url"), "www.x.in").is_err());
        assert!(from_text(field_of("agency.txt", "agency_timezone"), "Asia/Kolkata").is_ok());
        assert!(from_text(field_of("agency.txt", "agency_timezone"), "IST").is_err());
        assert!(from_text(field_of("agency.txt", "agency_lang"), "ta-IN").is_ok());
        assert!(from_text(field_of("feed_info.txt", "feed_lang"), "english").is_err());
    }

    #[test]
    fn the_api_takes_numbers_and_text_and_gives_times_back_as_text() {
        let start = field_of("timeframes.txt", "start_time");
        assert_eq!(
            from_api(start, &json!("7:00:00")).unwrap(),
            Some(json!(25200))
        );
        assert_eq!(from_api(start, &json!(25200)).unwrap(), Some(json!(25200)));
        assert_eq!(to_api(start, &Some(json!(25200))), json!("07:00:00"));
        let len = field_of("pathways.txt", "length");
        assert_eq!(from_api(len, &json!(12)).unwrap(), Some(json!(12.0)));
        assert_eq!(from_api(len, &json!("")).unwrap(), None);
        assert_eq!(from_api(len, &Value::Null).unwrap(), None);
        assert!(from_api(len, &json!([1])).is_err());
        assert_eq!(
            to_text(len, &Some(json!(12.0))),
            "12",
            "a whole float is written without its fraction"
        );
    }

    #[test]
    fn the_spec_json_lists_every_file_with_its_storage() {
        let v = spec_json();
        let files = v["files"].as_array().unwrap();
        assert_eq!(files.len(), FILES.len());
        let pathways = files.iter().find(|f| f["file"] == "pathways.txt").unwrap();
        assert_eq!(pathways["storage"], "record");
        assert_eq!(pathways["entity"], "pathway");
        assert_eq!(pathways["key"]["field"], "pathway_id");
        let stops = files.iter().find(|f| f["file"] == "stops.txt").unwrap();
        assert_eq!(stops["storage"], "bespoke");
        let lt = stops["fields"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["name"] == "location_type")
            .unwrap();
        assert_eq!(lt["type"], "enum");
        assert_eq!(lt["values"].as_array().unwrap().len(), 5);
    }
}
