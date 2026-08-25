@0x945b5ced7b501213;

struct Message {
  flag @0 :Bool;
  label @1 :Text;
  blob @2 :Data;
  stamp @3 :Timestamp;
  tiny @4 :UInt8;
  small @5 :UInt16;
  medium @6 :UInt32;
  big @7 :UInt64;
  tinySigned @8 :Int8;
  smallSigned @9 :Int16;
  mediumSigned @10 :Int32;
  bigSigned @11 :Int64;
  ratio @12 :Float32;
  precise @13 :Float64;
  note @14 :Text;
  attachment @15 :Data;
  seenAt @16 :Timestamp;
  checksum @17 :Data;
  pixels @18 :Data;
  gains @19 :List(Float32);
  offsets @20 :List(Int16);
  flags @21 :List(Bool);
  counters @22 :List(UInt64);
  deltas @23 :List(Int64);
  weights @24 :List(Float64);
  tags @25 :List(Text);
  chunks @26 :List(Data);
  pose @27 :Pose;
  profile @28 :Profile;
  samples @29 :List(SamplesItem);
  maybePose @30 :Maybepose;
  maybeTags @31 :List(Text);

  struct Pose {
    xM @0 :Float64;
    yM @1 :Float64;
    frame @2 :Text;
  }
  struct Profile {
    gamma @0 :Float64;
    whiteBalance @1 :Whitebalance;

    struct Whitebalance {
      red @0 :Float32;
      blue @1 :Float32;
    }
  }
  struct SamplesItem {
    offset @0 :Int16;
    value @1 :Float64;
    label @2 :Text;
    takenAt @3 :Timestamp;
    history @4 :List(UInt32);
  }
  struct Maybepose {
    xM @0 :Float64;
  }
}

struct Timestamp {
  sec @0 :Int64;
  nsec @1 :UInt32;
}
