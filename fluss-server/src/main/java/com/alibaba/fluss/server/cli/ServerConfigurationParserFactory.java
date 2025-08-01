/*
 * Licensed to the Apache Software Foundation (ASF) under one or more
 * contributor license agreements.  See the NOTICE file distributed with
 * this work for additional information regarding copyright ownership.
 * The ASF licenses this file to You under the Apache License, Version 2.0
 * (the "License"); you may not use this file except in compliance with
 * the License.  You may obtain a copy of the License at
 *
 *    http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

package com.alibaba.fluss.server.cli;

import org.apache.commons.cli.CommandLine;
import org.apache.commons.cli.Options;

import javax.annotation.Nonnull;

import java.util.Properties;

/**
 * Parser factory which generates a {@link ServerConfiguration} from the given list of command line
 * arguments.
 */
public class ServerConfigurationParserFactory implements ParserResultFactory<ServerConfiguration> {

    public static Options options() {
        final Options options = new Options();
        options.addOption(CommandLineOptions.CONFIG_DIR_OPTION);
        options.addOption(CommandLineOptions.DYNAMIC_PROPERTY_OPTION);

        return options;
    }

    @Override
    public Options getOptions() {
        return options();
    }

    @Override
    public ServerConfiguration createResult(@Nonnull CommandLine commandLine) {
        final String configDir =
                commandLine.getOptionValue(CommandLineOptions.CONFIG_DIR_OPTION.getOpt());

        final Properties dynamicProperties =
                commandLine.getOptionProperties(
                        CommandLineOptions.DYNAMIC_PROPERTY_OPTION.getOpt());

        return new ServerConfiguration(configDir, dynamicProperties, commandLine.getArgs());
    }
}
