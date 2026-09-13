#!/bin/sh
# Synthetic process fixture only. Never used by deploy/detector/Dockerfile.
case "$5" in
  rm) exit 0 ;;
  image) printf 'sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n'; exit 0 ;;
  run)
    for arg in "$@"; do
      case "$arg" in
        /usr/bin/clamscan) step=antivirus ;;
        /usr/bin/ffprobe) step=probe ;;
        /usr/bin/ffmpeg) step=decode ;;
      esac
    done
    printf '%s\n' "$step" >> "$(dirname "$0")/executions"
    case "$step" in
      antivirus) printf '/input/asset: OK\n' ;;
      probe) printf '{"format":{"duration":"1.0","format_name":"mov,mp4,m4a,3gp,3g2,mj2","tags":{"major_brand":"isom"}},"streams":[{"codec_type":"video","codec_name":"h264","width":64,"height":64,"avg_frame_rate":"25/1"}]}\n' ;;
      decode) printf 'frame=25\nout_time_us=1000000\nprogress=end\n' ;;
      *) exit 2 ;;
    esac ;;
  *) exit 2 ;;
esac
