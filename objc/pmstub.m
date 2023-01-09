#import "IOPMLibPrivate.h"
#import <Foundation/Foundation.h>
#import <IOKit/pwr_mgt/IOPMLib.h>

int setSleepDisabled(bool sleepDisabled) {
  CFBooleanRef sleepDisabledBool = sleepDisabled ? kCFBooleanTrue : kCFBooleanFalse;
  IOReturn result = IOPMSetSystemPowerSetting(kIOPMSleepDisabledKey, sleepDisabledBool);
  return result;
}

bool getSleepDisabled() {
  CFDictionaryRef settings = IOPMCopySystemPowerSettings();
  CFBooleanRef sleepDisabled = CFDictionaryGetValue(settings, kIOPMSleepDisabledKey);
  return sleepDisabled == kCFBooleanTrue;
}
