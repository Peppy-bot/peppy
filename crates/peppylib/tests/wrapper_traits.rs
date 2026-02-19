use peppylib::{Deserialize, JsonSchema, Serialize};

#[derive(Serialize, Deserialize, JsonSchema)]
// If the macros require `serde` or `schemars` to be in scope, this might fail unless we configure them
// or if peppylib re-exports them in a way the macro can find.
#[serde(crate = "peppylib::serde")]
#[schemars(crate = "peppylib::schemars")]
struct TestStruct {
    field: String,
    count: i32,
}

#[test]
fn test_serialization() {
    let _s = TestStruct {
        field: "hello".to_string(),
        count: 42,
    };

    // Test that it implements the traits (via blanket impl)
    fn assert_serialize<T: peppylib::Serialize>() {}
    fn assert_deserialize<T: peppylib::DeserializeOwned>() {}
    fn assert_json_schema<T: peppylib::JsonSchema>() {}

    assert_serialize::<TestStruct>();
    assert_deserialize::<TestStruct>();
    assert_json_schema::<TestStruct>();
}
