
# Selenium test settings (see http://wiki.whamcloud.com/display/HYD/Running+Selenium+tests)

#: The URL to your Chroma server e.g. http://localhost:8000/ui/
CHROMA_URL = ""

#: Choice of browser (Chrome or Firefox)
BROWSER = 'Chrome'

#: User credentials for login to Chroma server
USERS = [{'username': 'admin', 'password': 'password', 'is_superuser': True}]

#: Run the tests using a virtual display instead of opening a window
HEADLESS = True
