/*  Copyright 2022-23, Juspay India Pvt Ltd
    This program is free software: you can redistribute it and/or modify it under the terms of the GNU Affero General Public License
    as published by the Free Software Foundation, either version 3 of the License, or (at your option) any later version. This program
    is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY; without even the implied warranty of MERCHANTABILITY
    or FITNESS FOR A PARTICULAR PURPOSE. See the GNU Affero General Public License for more details. You should have received a copy of
    the GNU Affero General Public License along with this program. If not, see <https://www.gnu.org/licenses/>.
*/

use actix_web_prom::PrometheusMetrics;
// INCOMING_API (http_request_duration_seconds) and the incoming_api! macro come from shared,
// which registers the metric itself. Defining it here as well panics at startup with AlreadyReg.
pub use shared::tools::prometheus::*;

/// Returns the `PrometheusMetrics` instance for the app, serving `/metrics`.
///
/// All the metrics, including incoming API requests, are registered by
/// `shared::tools::prometheus::init_prometheus_metrics`.
pub fn prometheus_metrics() -> PrometheusMetrics {
    init_prometheus_metrics()
}
