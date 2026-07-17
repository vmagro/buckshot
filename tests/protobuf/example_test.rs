use example::Example;
use prost::Name;

#[test]
fn test_example_name() {
    assert_eq!("tests.protobuf.example.Example", Example::full_name());
}
