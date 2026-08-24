@0xf6ca01ad72fa67b7;

struct Message {
  frame @0 :Data;
  encoding @1 :Text;
  width @2 :UInt16;
  height @3 :UInt16;
  stamp @4 :Timestamp;
}

struct Timestamp {
  sec @0 :Int64;
  nsec @1 :UInt32;
}
