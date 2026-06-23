@0xb7a7c9b8f5a0d4f3;

struct ImageMessage {
  header  @0 :Header;
  encoding @1 :Text;    # e.g., "rgb8", "bgr8", "yuyv", "mjpeg"
  width   @2 :UInt32;
  height  @3 :UInt32;
  image   @4 :Data;     # bytes (u8). For RGB, expect width*height*3.

  struct Header {
    stamp   @0 :Time;   # analogous to ROS time
    frameId @1 :UInt32;
  }

  struct Time {
    sec  @0 :Int64;     # seconds since epoch
    nsec @1 :UInt32;    # nanoseconds offset
  }
}
