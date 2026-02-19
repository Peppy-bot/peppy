use peppylib::derive::{Deserialize, JsonSchema, Serialize};

#[derive(Serialize, Deserialize, JsonSchema)]
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
