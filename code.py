import board
import busio
import time
from digitalio import DigitalInOut
import adafruit_requests as requests
from adafruit_esp32spi import adafruit_esp32spi
from adafruit_esp32spi import adafruit_esp32spi_wifimanager
import adafruit_character_lcd.character_lcd as characterlcd
from adafruit_datetime import datetime

print("Initializing...")

lcd_rs = DigitalInOut(board.GP16)
lcd_en = DigitalInOut(board.GP17)
lcd_d7 = DigitalInOut(board.GP26)
lcd_d6 = DigitalInOut(board.GP22)
lcd_d5 = DigitalInOut(board.GP21)
lcd_d4 = DigitalInOut(board.GP20)

lcd_columns = 20
lcd_rows = 4

lcd = characterlcd.Character_LCD_Mono(
    lcd_rs, lcd_en, lcd_d4, lcd_d5, lcd_d6, lcd_d7, lcd_columns, lcd_rows
)

lcd.message = "Initializing..."

# Get wifi details and more from a secrets.py file
try:
    from secrets import secrets
except ImportError:
    print("WiFi secrets are kept in secrets.py, please add them there!")
    raise

WEATHER_URL = secrets["weather_url"]
TIME_URL = secrets["time_url"]

TIME_UPDATE_INTERVAL_SECS = 5  # this is also the loop interval
WEATHER_UPDATE_INTERVAL_SECS = 30
MAX_SUCCESSIVE_WEATHER_ERRORS = 3

WIFI_RESET_PAUSE_SECS = 2

DAYS = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"]
MONTHS = [
    "Jan",
    "Feb",
    "Mar",
    "Apr",
    "May",
    "Jun",
    "Jul",
    "Aug",
    "Sep",
    "Oct",
    "Nov",
    "Dec",
]

esp32_cs = DigitalInOut(board.GP9)
esp32_ready = DigitalInOut(board.GP7)
esp32_reset = DigitalInOut(board.GP6)

spi = busio.SPI(clock=board.GP10, MOSI=board.GP11, MISO=board.GP8)

esp = adafruit_esp32spi.ESP_SPIcontrol(spi, esp32_cs, esp32_ready, esp32_reset)
wifi = adafruit_esp32spi_wifimanager.ESPSPI_WiFiManager(esp, secrets)

success_counter = 0
error_counter = 0
successive_weather_errors = 0


def get_time():
    global success_counter
    global error_counter

    try:
        print("Updating time... ", end="")
        response = wifi.get(TIME_URL)
        rj = response.json()
        response.close()
        success_counter = success_counter + 1

        dt = rj.split("T")
        d = dt[0].split("-")

        yyyy = int(d[0])
        mm = int(d[1])
        dd = int(d[2])

        t = (dt[1].split("."))[0].split(":")
        h = int(t[0])
        m = int(t[1])
        s = int(t[2])

        print("OK")
        return datetime(yyyy, mm, dd, h, m, s)
    except (ValueError, RuntimeError) as e:
        print("Failed to get time, retrying\n", e)

        wifi.reset()
        print("Sleeping {}s to allow wifi reset".format(WIFI_RESET_PAUSE_SECS))
        time.sleep(WIFI_RESET_PAUSE_SECS)

        error_counter = error_counter + 1
        return None


def get_weather():
    global success_counter
    global error_counter
    global successive_weather_errors

    try:
        print("Updating weather... ", end="")
        response = wifi.get(WEATHER_URL)
        wj = response.json()
        response.close()
        success_counter = success_counter + 1
        successive_weather_errors = 0

        print("OK")
        return wj
    except (ValueError, RuntimeError) as e:
        print("Failed to get weather, retrying\n", e)

        wifi.reset()
        print("Sleeping {}s to allow wifi reset".format(WIFI_RESET_PAUSE_SECS))
        time.sleep(WIFI_RESET_PAUSE_SECS)

        successive_weather_errors += 1
        error_counter = error_counter + 1
        return None


def truncate(desc, length):
    if len(desc) <= length:
        return desc
    return desc[0] + "'" + desc[len(desc) - length + 2 : len(desc)]


deg_symbol = "".join(map(chr, bytes([223])))

last_weather_time = None
last_weather = None
while last_weather == None:
    last_weather = get_weather()
    last_weather_time = time.monotonic()

print("Initializing complete.")

while True:
    now = time.monotonic()

    if last_weather_time == None or (
        now - last_weather_time > WEATHER_UPDATE_INTERVAL_SECS
    ):
        print(
            "more than {:d}s since last weather.".format(WEATHER_UPDATE_INTERVAL_SECS)
        )
        maybe_weather = get_weather()
        if (
            maybe_weather == None
            and successive_weather_errors > MAX_SUCCESSIVE_WEATHER_ERRORS
        ):
            last_weather = None
        if maybe_weather != None:
            last_weather = maybe_weather
            last_weather_time = now

    if last_weather == None:
        weather_desc = "WEATHER"
        weather_temp_str = "ERR"
    else:
        weather_desc = last_weather["weather"][0]["main"]
        temperature = last_weather["main"]["temp"]
        weather_temp_str = "{}{}F".format(int(temperature), deg_symbol)

    dt = get_time()
    if dt == None:
        lcd.clear()
        lcd.message = "TIME ERROR"
        continue

    d = dt.date()
    t = dt.time()

    day = DAYS[d.weekday()]
    month = MONTHS[d.month - 1]

    lcd.clear()
    lcd.message = "{:02d}:{:02d}{:>15}\n{} {} {}{:>11}".format(
        t.hour, t.minute, truncate(weather_desc, 7), day, month, d.day, weather_temp_str
    )

    print("success: ", success_counter, "- errors: ", error_counter)

    print("sleeping for {}s".format(TIME_UPDATE_INTERVAL_SECS))
    time.sleep(TIME_UPDATE_INTERVAL_SECS)
