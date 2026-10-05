#!/bin/bash
# M5 echo test on a real phone: an app in the session records from
# farsight-mic while the session plays pink noise (the far end), which the
# phone plays through its speaker and hears again through its microphone.
# Run once with the phone's echo cancellation and once without, and
# compare what came back.
#
#   tools/m5/phone-echo.sh OUTDIR ADDRESS [SERIAL]
#
# Needs a farsight-server on ADDRESS whose desktop shows a terminal
# (e.g. -- labwc -s xfce4-terminal), the phone's key in its
# authorized_keys, the app installed, and the phone unlocked. adb types
# into that terminal; the server and this script share OUTDIR, so the
# server must run on this machine.
#
# Prints the recording's level per half second: the far end plays from
# 5 s to 13 s (windows 10 to 26).
set -e
OUT=$(mkdir -p "$1" && cd "$1" && pwd)
ADDRESS=$2
ADB=(adb ${3:+-s "$3"})

ffmpeg -loglevel error -y -f lavfi -i "anoisesrc=color=pink:amplitude=0.25:sample_rate=48000:duration=8" \
	-ac 2 "$OUT/far.wav"
cat > "$OUT/echo.sh" <<EOF
#!/bin/sh
rm -f $OUT/rec.wav
pw-record --target farsight-mic --channels 1 $OUT/rec.wav &
R=\$!
sleep 5
pw-play $OUT/far.wav
sleep 3
kill \$R
EOF

levels() {
	ffmpeg -nostats -i "$1" -af "highpass=f=100,asetnsamples=24000,astats=metadata=1:reset=1,ametadata=print:key=lavfi.astats.Overall.RMS_level" \
		-f null - 2>&1 | grep -o 'RMS_level=[-0-9.inf]*' | cut -d= -f2 | xargs printf '%.0f '
}

for cancel in false true; do
	"${ADB[@]}" shell "input keyevent KEYCODE_WAKEUP; wm dismiss-keyguard; am force-stop dev.fanchao.farsight;
		am start -n dev.fanchao.farsight/.MainActivity --es address $ADDRESS --ez echo_cancel $cancel --es mic ALWAYS" > /dev/null
	sleep 9
	"${ADB[@]}" shell "input tap 1200 600; sleep 0.3; input text 'sh $OUT/echo.sh'; input keyevent KEYCODE_ENTER"
	sleep 20
	name=$([ $cancel = true ] && echo cancelled || echo raw)
	cp "$OUT/rec.wav" "$OUT/$name.wav"
	echo "$name: $(levels "$OUT/$name.wav")"
done
