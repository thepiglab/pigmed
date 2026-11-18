#!/usr/bin/env bash
# Build the FFmpeg release that the card runtime decodes with (PyAV/torchcodec bundle: FFmpeg 9.0.2), so the Rust
# runner's ffmpeg/ffprobe produce the same decoded audio/video. Static CLI under $EMBED_ROOT/ffmpeg9/bin.
#   EMBED_ENV=<site env> [FFMPEG_VERSION=9.0.2] sbatch --export=ALL examples/slurm/embed-rs-ffmpeg.sbatch
#SBATCH --job-name=pigmed-embed-ffmpeg
#SBATCH --nodes=1
#SBATCH --ntasks=1
#SBATCH --cpus-per-task=32
#SBATCH --mem=32G
#SBATCH --time=02:00:00
set -euo pipefail
source "${EMBED_ENV:?export EMBED_ENV=<site env>}"
V=${FFMPEG_VERSION:-9.0.2}; NASM=2.16.03
P=$EMBED_ROOT/ffmpeg9; W=${TMPDIR:-/tmp}/ffbuild-$SLURM_JOB_ID; mkdir -p $W $P; cd $W
curl -sSfL -o nasm.tar.xz https://www.nasm.us/pub/nasm/releasebuilds/$NASM/nasm-$NASM.tar.xz
tar xf nasm.tar.xz && (cd nasm-$NASM && ./configure --prefix=$W/tools >/dev/null && make -j32 >/dev/null && make install >/dev/null)
export PATH=$W/tools/bin:$PATH; nasm -v
curl -sSfL -o ffmpeg.tar.xz https://ffmpeg.org/releases/ffmpeg-$V.tar.xz
tar xf ffmpeg.tar.xz && cd ffmpeg-$V
./configure --prefix=$P --disable-doc --disable-debug --disable-shared --enable-static --disable-network \
  --disable-autodetect --disable-ffplay > configure.log 2>&1 || { tail -30 configure.log; exit 1; }
make -j32 > make.log 2>&1 || { tail -40 make.log; exit 1; }
make install > /dev/null
$P/bin/ffmpeg -hide_banner -version | head -2
$P/bin/ffmpeg -hide_banner -decoders 2>/dev/null | grep -E " (aac|mp3float|vorbis|opus|flac|h264|mpeg4|mjpeg|vp9|hevc) " | head
rm -rf $W
