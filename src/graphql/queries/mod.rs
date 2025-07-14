use crate::graphql::types::TripQueryVariables;

pub const TRIP_QUERY: &str = r#"
query TipPlan($trip_id: String!, $service_date: String!) {
  trip(id: $trip_id) {
    gtfsId
    route {
      id
      gtfsId
      longName
    }
    stoptimesForDate(serviceDate: $service_date) {
      stop {
        id
        code
        name
        lat
        lon  
      }
      stopPosition
      realtimeArrival
      scheduledArrival
      scheduledDeparture
      realtimeDeparture
    }
  }
}
"#;

pub fn get_trip_query(variables: TripQueryVariables) -> (String, serde_json::Value) {
    let variables_json = serde_json::to_value(variables).unwrap();
    (TRIP_QUERY.to_string(), variables_json)
}